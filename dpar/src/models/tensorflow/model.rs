use std::f32;
use std::ops::{Deref, DerefMut};
use std::path::Path;

use enum_map::EnumMap;
use tensorflow::{
    Graph, ImportGraphDefOptions, Operation, Session, SessionOptions, SessionRunArgs, Tensor,
};

use features::{InputVectorizer, Layer, LayerLookups};
use system::{ParserState, Transition, TransitionSystem};
use {ErrorKind, Result};

mod opnames {
    pub static INIT: &str = "init";

    pub static RESTORE: &str = "save/restore_all";
    pub static SAVE: &str = "save/control_dependency";
    pub static SAVE_FILE_PATH: &str = "save/Const";

    pub static IS_TRAINING: &str = "model/is_training";
    pub static LR: &str = "model/lr";

    pub static TARGETS: &str = "model/targets";

    pub static ACCURACY: &str = "model/accuracy";
    pub static LOGITS: &str = "model/logits";
    pub static LOSS: &str = "model/loss";

    pub static TRAIN: &str = "model/train";
}

pub struct LayerTensors(pub EnumMap<Layer, TensorWrap>);

impl LayerTensors {
    /// Extract for each layer the slice corresponding to the `idx`-th
    /// instance from the batch.
    pub fn to_instance_slices(&mut self, idx: usize) -> EnumMap<Layer, &mut [i32]> {
        let mut slices = EnumMap::new();

        for (layer, tensor) in self.iter_mut() {
            let layer_size = tensor.dims()[1] as usize;
            let offset = idx * layer_size;
            slices[layer] = &mut tensor[offset..offset + layer_size];
        }

        slices
    }
}

impl Deref for LayerTensors {
    type Target = EnumMap<Layer, TensorWrap>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for LayerTensors {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

/// Layer op in the parsing model
///
/// The parser can use several information layers, such as tokens, tags, and
/// dependency relations. Such layers have to be mapped to concrete Tensorflow
/// graph ops. This data structure is used to define a mapping from one
/// particular layer to a Tensorflow placeholder graph op.
pub enum LayerOp<T> {
    /// Op for layers using pre-trained embeddings
    ///
    /// For the use of pre-trained embeddings, we need to ops:
    ///
    /// * The placeholder op for the layer vector.
    /// * The placeholder op for the embedding matrix.
    Embedding { op: T, embed_op: T },

    /// Op for layers using a lookup table.
    ///
    /// The given op is the placeholder op for the layer vector.
    Table { op: T },
}

impl<S> LayerOp<S>
where
    S: AsRef<str>,
{
    /// Convert a graph op identifier to a graph op.
    fn to_graph_op(&self, graph: &Graph) -> Result<LayerOp<Operation>> {
        match self {
            &LayerOp::Embedding {
                ref op,
                ref embed_op,
            } => Ok(LayerOp::Embedding {
                op: graph.operation_by_name_required(op.as_ref())?,
                embed_op: graph.operation_by_name_required(embed_op.as_ref())?,
            }),
            &LayerOp::Table { ref op } => Ok(LayerOp::Table {
                op: graph.operation_by_name_required(op.as_ref())?,
            }),
        }
    }
}

/// A bundling of ops for different layers.
pub struct LayerOps<S>(EnumMap<Layer, Option<LayerOp<S>>>);

impl<S> LayerOps<S>
where
    S: AsRef<str>,
{
    /// Convert a graph op identifiers for all layers to a graph ops.
    fn to_graph_ops(&self, graph: &Graph) -> Result<LayerOps<Operation>> {
        let mut graph_ops = EnumMap::new();

        for (layer, op_name) in &self.0 {
            let op_name = ok_or_continue!(op_name.as_ref());
            graph_ops[layer] = Some(op_name.to_graph_op(graph)?);
        }

        Ok(LayerOps(graph_ops))
    }
}

impl<S> LayerOps<S> {
    /// Construct a new `LayerOps`.
    ///
    /// By default, the op for every layer is set to `None`.
    pub fn new() -> Self {
        LayerOps(EnumMap::new())
    }

    /// Set the op for a layer.
    pub fn insert(&mut self, layer: Layer, op: LayerOp<S>) {
        self.0[layer] = Some(op);
    }

    /// Get the op for a layer.
    pub fn layer_lookup(&self, layer: Layer) -> Option<&LayerOp<S>> {
        self.0[layer].as_ref()
    }
}

/// Simple wrapper for `Tensor` that implements `Default` tensors.
pub struct TensorWrap(pub Tensor<i32>);

impl Default for TensorWrap {
    fn default() -> Self {
        TensorWrap(Tensor::new(&[]))
    }
}

impl From<Tensor<i32>> for TensorWrap {
    fn from(tensor: Tensor<i32>) -> Self {
        TensorWrap(tensor)
    }
}

impl Deref for TensorWrap {
    type Target = Tensor<i32>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for TensorWrap {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

/// Parser guide that uses a Tensorflow graph and model.
pub struct TensorflowModel<T>
where
    T: TransitionSystem,
{
    session: Session,
    system: T,
    vectorizer: InputVectorizer,
    layer_ops: LayerOps<Operation>,
    init_op: Operation,
    restore_op: Operation,
    save_op: Operation,
    save_file_path_op: Operation,
    lr_op: Operation,
    is_training_op: Operation,
    accuracy_op: Operation,
    logits_op: Operation,
    loss_op: Operation,
    targets_op: Operation,
    train_op: Operation,
}

impl<T> TensorflowModel<T>
where
    T: TransitionSystem,
{
    /// Load a Tensorflow graph.
    ///
    /// This constructor will use the graphs's initializer to initialize the
    /// graph's parameters.
    pub fn load_graph<S>(
        config_protobuf: &[u8],
        model_protobuf: &[u8],
        system: T,
        vectorizer: InputVectorizer,
        op_names: &LayerOps<S>,
    ) -> Result<Self>
    where
        S: AsRef<str>,
    {
        let mut model = Self::load_graph_(
            config_protobuf,
            model_protobuf,
            system,
            vectorizer,
            op_names,
        )?;

        // Initialize parameters.
        let mut args = SessionRunArgs::new();
        args.add_target(&model.init_op);
        model
            .session
            .run(&mut args)
            .expect("Cannot initialize parameters");

        Ok(model)
    }

    pub fn load_graph_with_parameters<P, S>(
        config_protobuf: &[u8],
        model_protobuf: &[u8],
        parameters_path: P,
        system: T,
        vectorizer: InputVectorizer,
        op_names: &LayerOps<S>,
    ) -> Result<Self>
    where
        P: AsRef<Path>,
        S: AsRef<str>,
    {
        let mut model = Self::load_graph_(
            config_protobuf,
            model_protobuf,
            system,
            vectorizer,
            op_names,
        )?;

        // Restore parameters.
        let path_tensor = prepare_path(parameters_path)?.into();
        let mut args = SessionRunArgs::new();
        args.add_feed(&model.save_file_path_op, 0, &path_tensor);
        args.add_target(&model.restore_op);
        model.session.run(&mut args)?;

        Ok(model)
    }

    /// Load a Tensorflow graph.
    ///
    /// This should be a frozen graph --- a graph in which each variable is
    /// converted to a constant. A graph can be frozen using Tensorflow's
    /// [freeze_graph.py](https://github.com/tensorflow/tensorflow/blob/master/tensorflow/python/tools/freeze_graph.py)
    /// script.
    fn load_graph_<S>(
        config_protobuf: &[u8],
        model_protobuf: &[u8],
        system: T,
        vectorizer: InputVectorizer,
        op_names: &LayerOps<S>,
    ) -> Result<Self>
    where
        S: AsRef<str>,
    {
        let opts = ImportGraphDefOptions::new();
        let mut graph = Graph::new();
        graph.import_graph_def(model_protobuf, &opts)?;

        let mut session_opts = SessionOptions::new();
        session_opts.set_config(config_protobuf)?;
        let session = Session::new(&session_opts, &graph)?;

        let layer_ops = op_names.to_graph_ops(&graph)?;

        let init_op = graph.operation_by_name_required(opnames::INIT)?;
        let restore_op = graph.operation_by_name_required(opnames::RESTORE)?;
        let save_op = graph.operation_by_name_required(opnames::SAVE)?;
        let save_file_path_op = graph.operation_by_name_required(opnames::SAVE_FILE_PATH)?;

        let is_training_op = graph.operation_by_name_required(opnames::IS_TRAINING)?;
        let lr_op = graph.operation_by_name_required(opnames::LR)?;

        let accuracy_op = graph.operation_by_name_required(opnames::ACCURACY)?;
        let logits_op = graph.operation_by_name_required(opnames::LOGITS)?;
        let loss_op = graph.operation_by_name_required(opnames::LOSS)?;
        let targets_op = graph.operation_by_name_required(opnames::TARGETS)?;

        let train_op = graph.operation_by_name_required(opnames::TRAIN)?;

        Ok(TensorflowModel {
            system,
            session,
            vectorizer,
            layer_ops,
            init_op,
            restore_op,
            save_op,
            save_file_path_op,
            is_training_op,
            lr_op,
            accuracy_op,
            logits_op,
            loss_op,
            targets_op,
            train_op,
        })
    }

    /// Find the best transition given a slice of transition logits.
    pub(crate) fn logits_best_transition<S>(&self, state: &ParserState, logits: S) -> T::T
    where
        S: AsRef<[f32]>,
    {
        // Invariant: we should have as many predictions as transitions.
        let n_predictions = logits.as_ref().len();
        let n_transitions = self.system.transitions().len();
        assert_eq!(
            n_predictions, n_transitions,
            "Number of transitions ({}) and predictions ({}) are inequal.",
            n_transitions, n_predictions
        );

        let mut best = self.system.transitions().value(1).unwrap();
        let mut best_score = f32::NEG_INFINITY;

        for (idx, logit) in logits.as_ref().iter().enumerate() {
            if idx == 0 {
                continue;
            }

            if *logit > best_score {
                let transition = self
                    .system
                    .transitions()
                    .value(idx)
                    .expect("Invalid transition index.");
                if transition.is_possible(state) {
                    best = transition;
                    best_score = *logit;
                }
            }
        }

        best.clone()
    }

    /// Predict transitions, returning their logits.
    pub fn predict_logits(&mut self, input_tensors: &LayerTensors) -> Tensor<f32> {
        let mut is_training = Tensor::new(&[]);
        is_training[0] = false;

        let mut args = SessionRunArgs::new();
        args.add_feed(&self.is_training_op, 0, &is_training);
        add_to_args(
            &mut args,
            &self.layer_ops,
            self.vectorizer.layer_lookups(),
            &input_tensors,
        );
        let logits_token = args.request_fetch(&self.logits_op, 0);
        self.session.run(&mut args).expect("Cannot run graph");

        args.fetch(logits_token).expect("Unable to retrieve output")
    }

    pub fn save<P>(&mut self, path: P) -> Result<()>
    where
        P: AsRef<Path>,
    {
        // Add leading directory component if absent.
        let path_tensor = prepare_path(path)?.into();

        // Call the save op.
        let mut args = SessionRunArgs::new();
        args.add_feed(&self.save_file_path_op, 0, &path_tensor);
        args.add_target(&self.save_op);
        self.session.run(&mut args).map_err(|s| s.into())
    }

    pub fn train(&mut self, input_tensors: &LayerTensors, targets: &Tensor<i32>) -> (f32, f32) {
        let mut is_training = Tensor::new(&[]);
        is_training[0] = true;

        let mut lr = Tensor::new(&[]);
        lr[0] = 0.05f32;

        let mut args = SessionRunArgs::new();
        args.add_feed(&self.is_training_op, 0, &is_training);
        args.add_feed(&self.lr_op, 0, &lr);
        args.add_target(&self.train_op);

        self.validate_(args, input_tensors, targets)
    }

    pub fn validate(&mut self, input_tensors: &LayerTensors, targets: &Tensor<i32>) -> (f32, f32) {
        let mut is_training = Tensor::new(&[]);
        is_training[0] = false;

        let mut args = SessionRunArgs::new();
        args.add_feed(&self.is_training_op, 0, &is_training);
        self.validate_(args, input_tensors, targets)
    }

    pub fn validate_<'l>(
        &'l mut self,
        mut args: SessionRunArgs<'l>,
        input_tensors: &'l LayerTensors,
        targets: &'l Tensor<i32>,
    ) -> (f32, f32) {
        // Add inputs.
        add_to_args(
            &mut args,
            &self.layer_ops,
            self.vectorizer.layer_lookups(),
            input_tensors,
        );

        // Add gold labels.
        args.add_feed(&self.targets_op, 0, targets);

        let accuracy_token = args.request_fetch(&self.accuracy_op, 0);
        let loss_token = args.request_fetch(&self.loss_op, 0);

        self.session.run(&mut args).expect("Cannot run graph");

        (
            args.fetch(loss_token).expect("Unable to retrieve loss")[0],
            args.fetch(accuracy_token).expect("Unable to retrieve loss")[0],
        )
    }

    pub fn vectorizer(&self) -> &InputVectorizer {
        &self.vectorizer
    }
}

// Unfortunately, add_to_args cannot be a method of TensorflowModel with
// the following signature:
//
// add_to_args<'l>(&'a self, step: &mut SessionRunArgs<'l>, ...)
//
// Because args would hold a reference to &self, which disallows us to run
// the session, because session running requires &mut self. The following
// RFC would solve this:
//
// https://github.com/rust-lang/rfcs/issues/1215
//
// Another possibility would be to use interior mutability for the
// Tensorflow Session, but I'd like to avoid this.
pub(crate) fn add_to_args<'l>(
    args: &mut SessionRunArgs<'l>,
    layer_ops: &LayerOps<Operation>,
    layer_lookups: &'l LayerLookups,
    input_tensors: &'l LayerTensors,
) {
    for (layer, layer_op) in &layer_ops.0 {
        let layer_op = ok_or_continue!(layer_op.as_ref());

        match layer_op {
            &LayerOp::Embedding {
                ref op,
                ref embed_op,
            } => {
                // Fill the layer vector placeholder.
                args.add_feed(op, 0, &input_tensors[layer]);

                // Fill the embedding placeholder. If we have an op for
                // the embedding of a layer, there should always be a
                // corresponding embedding matrix.
                let embed_matrix = layer_lookups
                    .layer_lookup(layer)
                    .unwrap()
                    .embed_matrix()
                    .unwrap();
                args.add_feed(embed_op, 0, embed_matrix);
            }
            &LayerOp::Table { ref op } => {
                // Fill the layer vector placeholder.
                args.add_feed(op, 0, &input_tensors[layer]);
            }
        }
    }
}

/// Tensorflow requires a path that contains a directory component.
fn prepare_path<P>(path: P) -> Result<String>
where
    P: AsRef<Path>,
{
    let path = path.as_ref();
    let path = if path.components().count() == 1 {
        Path::new("./").join(path)
    } else {
        path.to_owned()
    };

    path.to_str()
        .ok_or(
            ErrorKind::FilenameEncodingError("Filename contains non-unicode characters".to_owned())
                .into(),
        ).map(ToOwned::to_owned)
}
