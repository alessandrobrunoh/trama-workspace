//! Strongly typed identifiers (ULID-backed). Never pass bare `String`s.

use serde::{Deserialize, Serialize};
use std::fmt;

macro_rules! id_type {
    ($name:ident) => {
        #[derive(
            Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
        )]
        pub struct $name(pub String);

        impl $name {
            pub fn generate() -> Self {
                Self(ulid::Ulid::new().to_string())
            }
        }

        impl From<&str> for $name {
            fn from(value: &str) -> Self {
                Self(value.to_string())
            }
        }

        impl From<String> for $name {
            fn from(value: String) -> Self {
                Self(value)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

id_type!(WorkspaceId);
id_type!(RepositoryId);
id_type!(ThreadId);
id_type!(CheckoutId);
id_type!(AgentSessionId);
id_type!(MessageId);
id_type!(CommentId);
id_type!(CheckpointId);
