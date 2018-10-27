extern crate conllx;
extern crate dpar;
#[macro_use]
extern crate dpar_utils;
extern crate getopts;
#[macro_use]
extern crate serde_derive;
extern crate stdinout;
extern crate toml;

use std::collections::BTreeSet;
use std::env::args;
use std::fs::File;
use std::io::{BufRead, Write};
use std::path::Path;
use std::process;

use conllx::{HeadProjectivizer, Projectivize, ReadSentence};
use dpar::features::addr::Layer::Char;
use dpar::features::{AddressedValues, InputVectorizer, Layer, Lookup};
use dpar::system::{sentence_to_dependencies, ParserState};
use dpar::systems::{
    ArcEagerSystem, ArcHybridSystem, ArcStandardSystem, StackProjectiveSystem, StackSwapSystem,
};
use dpar::train::{GreedyTrainer, NoopCollector};
use getopts::Options;
use stdinout::{Input, Output};

use dpar_utils::{Config, ErrorKind, OrExit, Result, SerializableTransitionSystem, TomlRead};

#[derive(Serialize)]
struct Shapes {
    batch_size: usize,
    tokens: usize,
    tags: usize,
    deprels: usize,
    features: usize,
    chars: usize,
    deprel_embeds: usize,
    n_features: usize,
    n_labels: usize,
    prefix_len: usize,
    suffix_len: usize,
}

fn print_usage(program: &str, opts: Options) {
    let brief = format!("Usage: {} [options] CONFIG TRAIN_DATA SHAPES", program);
    print!("{}", opts.usage(&brief));
}

fn main() {
    let args: Vec<String> = args().collect();
    let program = args[0].clone();

    let mut opts = Options::new();
    opts.optflag("h", "help", "print this help menu");
    let matches = opts.parse(&args[1..]).or_exit();

    if matches.opt_present("h") {
        print_usage(&program, opts);
        return;
    }

    if matches.free.is_empty() || matches.free.len() > 3 {
        print_usage(&program, opts);
        return;
    }

    let config_file = File::open(&matches.free[0]).or_exit();
    let mut config = Config::from_toml_read(config_file).or_exit();
    config.relativize_paths(&matches.free[0]).or_exit();

    let input = Input::from(matches.free.get(1));
    let reader = conllx::Reader::new(input.buf_read().or_exit());
    let output = Output::from(matches.free.get(2));
    let writer = output.write().or_exit();

    train(&config, reader, writer).or_exit();
}

fn train<R, W>(config: &Config, reader: conllx::Reader<R>, write: W) -> Result<()>
where
    R: BufRead,
    W: Write,
{
    match config.parser.system.as_ref() {
        "arceager" => train_with_system::<R, W, ArcEagerSystem>(config, reader, write),
        "archybrid" => train_with_system::<R, W, ArcHybridSystem>(config, reader, write),
        "arcstandard" => train_with_system::<R, W, ArcStandardSystem>(config, reader, write),
        "stackproj" => train_with_system::<R, W, StackProjectiveSystem>(config, reader, write),
        "stackswap" => train_with_system::<R, W, StackSwapSystem>(config, reader, write),
        _ => {
            stderr!("Unsupported transition system: {}", config.parser.system);
            process::exit(1);
        }
    }
}

fn train_with_system<R, W, S>(config: &Config, reader: conllx::Reader<R>, write: W) -> Result<()>
where
    R: BufRead,
    S: SerializableTransitionSystem,
    W: Write,
{
    let lookups = config.lookups.create_lookups()?;
    let inputs = config.parser.load_inputs()?;
    let vectorizer = InputVectorizer::new(lookups, inputs);
    let system: S = S::default();
    let collector = NoopCollector::new(system, vectorizer)?;
    let mut trainer = GreedyTrainer::new(collector);
    let projectivizer = HeadProjectivizer::new();

    for sentence in reader.sentences() {
        let sentence = if config.parser.pproj {
            projectivizer.projectivize(&sentence?)?
        } else {
            sentence?
        };

        let dependencies = sentence_to_dependencies(&sentence).or_exit();

        let mut state = ParserState::new(&sentence);
        trainer.parse_state(&dependencies, &mut state)?;
    }

    write_transition_system(&config, trainer.collector().transition_system())?;

    write_shapes(config, trainer, write)
}

fn write_shapes<W, S>(
    config: &Config,
    trainer: GreedyTrainer<S, NoopCollector<S>>,
    mut write: W,
) -> Result<()>
where
    W: Write,
    S: SerializableTransitionSystem,
{
    let vectorizer = trainer.collector().input_vectorizer();
    let layer_sizes = vectorizer.layer_sizes();
    let layer_lookups = vectorizer.layer_lookups();

    let (prefix_len, suffix_len) = affix_lengths(vectorizer.layer_addrs())?;

    let shapes = Shapes {
        batch_size: config.parser.train_batch_size,
        tokens: layer_sizes[Layer::Token],
        tags: layer_sizes[Layer::Tag],
        deprels: layer_sizes[Layer::DepRel],
        features: layer_sizes[Layer::Feature],
        chars: layer_sizes[Layer::Char],
        deprel_embeds: layer_lookups
            .layer_lookup(Layer::DepRel)
            .map(Lookup::len)
            .unwrap_or(0),
        n_features: layer_lookups
            .layer_lookup(Layer::Feature)
            .map(Lookup::len)
            .unwrap_or(0),
        n_labels: trainer.collector().transition_system().transitions().len(),
        prefix_len,
        suffix_len,
    };

    write!(write, "{}", toml::to_string(&shapes).or_exit());

    Ok(())
}

fn affix_lengths(addrs: &AddressedValues) -> Result<(usize, usize)> {
    let mut prefix_lens = BTreeSet::new();
    let mut suffix_lens = BTreeSet::new();
    for addr in &addrs.0 {
        if let Char(prefix_len, suffix_len) = addr.layer {
            prefix_lens.insert(prefix_len);
            suffix_lens.insert(suffix_len);
        }
    }

    if prefix_lens.len() != 1 || suffix_lens.len() != 1 {
        Err(ErrorKind::ConfigError(
            "Models with varying prefix or suffix lengths are not supported".into(),
        ).into())
    } else {
        Ok((
            prefix_lens.into_iter().next().unwrap(),
            suffix_lens.into_iter().next().unwrap(),
        ))
    }
}

fn write_transition_system<T>(config: &Config, system: &T) -> Result<()>
where
    T: SerializableTransitionSystem,
{
    let transitions_path = Path::new(&config.parser.transitions);
    let mut f = File::create(transitions_path)?;
    system.to_cbor_write(&mut f)?;
    Ok(())
}