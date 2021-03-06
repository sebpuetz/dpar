use conllx::Token;
use failure::Error;

use crate::guide::{BatchGuide, Guide};
use crate::parser::{Parse, ParseBatch};
use crate::system::{DependencySet, ParserState, Transition, TransitionSystem};

pub struct GreedyParser<G> {
    guide: G,
}

/// A greedy dependency parser.
///
/// This parser implements a deterministic/greedy dependency parser. See
/// Kübler, Nivre & McDonald, 2009, page 27 for a description of this type
/// of parser.
impl<G> GreedyParser<G>
where
    G: Guide,
{
    pub fn new(guide: G) -> Self {
        GreedyParser { guide }
    }
}

impl<G> Parse for GreedyParser<G>
where
    G: Guide,
{
    fn parse(&mut self, sentence: &[Token]) -> Result<DependencySet, Error> {
        let mut state = ParserState::new(sentence);

        while !<<G as Guide>::Transition as Transition>::S::is_terminal(&state) {
            self.guide.best_transition(&state).apply(&mut state);
        }

        Ok(state.dependencies())
    }
}

impl<G> ParseBatch for GreedyParser<G>
where
    G: BatchGuide,
{
    fn parse_batch<S>(&mut self, sentences: &[S]) -> Result<Vec<DependencySet>, Error>
    where
        S: AsRef<[Token]>,
    {
        let mut states: Vec<_> = sentences
            .iter()
            .map(|s| ParserState::new(s.as_ref()))
            .collect();

        loop {
            let (transitions, mapping) = {
                let mut active_states = Vec::new();
                let mut mapping = Vec::new();

                for (idx, state) in states.iter().enumerate() {
                    if !<<G as BatchGuide>::Transition as Transition>::S::is_terminal(state) {
                        active_states.push(state);
                        mapping.push(idx);
                    }
                }

                // We are done when all parser states are terminal.
                if active_states.is_empty() {
                    break;
                }

                (self.guide.best_transitions(&active_states), mapping)
            };

            // Apply transitions.
            for (idx, transition) in mapping.into_iter().zip(transitions) {
                transition.apply(&mut states[idx]);
            }
        }

        Ok(states.iter().map(ParserState::dependencies).collect())
    }
}
