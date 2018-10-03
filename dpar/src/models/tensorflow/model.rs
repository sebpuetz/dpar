use std::f32;

use enum_map::EnumMap;
use tensorflow::{
    Graph, ImportGraphDefOptions, Operation, Session, SessionOptions, SessionRunArgs, Tensor,
};

use features::{InputVectorizer, Layer, LayerLookups};
use system::{ParserState, Transition, TransitionSystem};
use Result;

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

/// Parser guide that uses a Tensorflow graph and model.
pub struct TensorflowModel<T>
where
    T: TransitionSystem,
{
    session: Session,
    system: T,
    vectorizer: InputVectorizer,
    layer_ops: LayerOps<Operation>,
    logits_op: Operation,
}

impl<T> TensorflowModel<T>
where
    T: TransitionSystem,
{
    /// Load a Tensorflow graph.
    ///
    /// This should be a frozen graph --- a graph in which each variable is
    /// converted to a constant. A graph can be frozen using Tensorflow's
    /// [freeze_graph.py](https://github.com/tensorflow/tensorflow/blob/master/tensorflow/python/tools/freeze_graph.py)
    /// script.
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
        let opts = ImportGraphDefOptions::new();
        let mut graph = Graph::new();
        graph.import_graph_def(model_protobuf, &opts)?;

        let mut session_opts = SessionOptions::new();
        session_opts.set_config(config_protobuf)?;
        let session = Session::new(&session_opts, &graph)?;

        let layer_ops = op_names.to_graph_ops(&graph)?;

        // Output
        let logits_op = graph.operation_by_name_required("prediction/model/logits")?;

        Ok(TensorflowModel {
            system,
            session,
            vectorizer,
            layer_ops,
            logits_op,
        })
    }

    /// Find the best transition given a slice of transition logits.
    pub(crate) fn logits_best_transition<S>(&self, state: &ParserState, logits: S) -> T::T
    where
        S: AsRef<[f32]>,
    {
        // Invariant: we should have as many predictions as transitions.
        let n_predictions = logits.as_ref().len();
        let n_transitions = self.system.transitions().len() + self.system.transitions().start_at();
        assert_eq!(
            n_predictions, n_transitions,
            "Number of transitions ({}) and predictions ({}) are inequal.",
            n_transitions, n_predictions
        );

        let mut best = self.system.transitions().value(0).unwrap();
        let mut best_score = f32::NEG_INFINITY;

        for (idx, logit) in logits.as_ref().iter().enumerate() {
            if *logit > best_score {
                let transition = self.system.transitions().value(idx).unwrap();
                if transition.is_possible(state) {
                    best = transition;
                    best_score = *logit;
                }
            }
        }

        best.clone()
    }

    /// Predict transitions, returning their logits.
    pub fn predict_logits(&mut self, input_tensors: &EnumMap<Layer, TensorWrap>) -> Tensor<f32> {
        let mut args = SessionRunArgs::new();
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

    pub fn vectorizer(&self) -> &InputVectorizer {
        &self.vectorizer
    }
}

// Unfortunately, add_to_args cannot be a method of TensorflowModel with
// the following signature:
//
// add_to_args<'a>(&'a self, step: &mut SessionRunArgs<'a>, ...)
//
// Because args would hold a reference to &self, which disallows us to run
// the session, because session running requires &mut self. The following
// RFC would solve this:
//
// https://github.com/rust-lang/rfcs/issues/1215
//
// Another possibility would be to use interior mutability for the
// Tensorflow Session, but I'd like to avoid this.
pub(crate) fn add_to_args<'a>(
    args: &mut SessionRunArgs<'a>,
    layer_ops: &LayerOps<Operation>,
    layer_lookups: &'a LayerLookups,
    input_tensors: &'a EnumMap<Layer, TensorWrap>,
) {
    for (layer, layer_op) in &layer_ops.0 {
        let layer_op = ok_or_continue!(layer_op.as_ref());

        match layer_op {
            &LayerOp::Embedding {
                ref op,
                ref embed_op,
            } => {
                // Fill the layer vector placeholder.
                args.add_feed(op, 0, &input_tensors[layer].0);

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
                args.add_feed(op, 0, &input_tensors[layer].0);
            }
        }
    }
}