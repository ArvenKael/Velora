//! Ways of talking to a model.
//!
//! Each provider hides its own transport. The engine above them only ever sees
//! [`crate::Provider`].

mod command;
mod openai;

pub use command::CommandProvider;
pub use openai::OpenAiCompatible;
