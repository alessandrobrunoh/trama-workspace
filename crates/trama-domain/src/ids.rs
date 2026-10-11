//! Strongly typed identifiers (ULID-backed). Never pass bare `String`s.

use serde::{Deserialize, Serialize};
use std::fmt;

macro_rules! id_type {
    ($name:ident) => {
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn assert_display_from_roundtrip<T>(id: &T)
    where
        T: Clone
            + fmt::Display
            + From<String>
            + for<'a> From<&'a str>
            + PartialEq
            + std::fmt::Debug,
    {
        let displayed = id.to_string();
        assert_eq!(T::from(displayed.as_str()), id.clone());
        assert_eq!(T::from(displayed.clone()), id.clone());
        assert_eq!(format!("{id}"), displayed);
        assert_eq!(displayed.len(), 26);
    }

    #[test]
    fn display_and_from_roundtrip_all_id_types() {
        assert_display_from_roundtrip(&WorkspaceId::generate());
        assert_display_from_roundtrip(&RepositoryId::generate());
        assert_display_from_roundtrip(&ThreadId::generate());
        assert_display_from_roundtrip(&CheckoutId::generate());
        assert_display_from_roundtrip(&AgentSessionId::generate());
        assert_display_from_roundtrip(&MessageId::generate());
        assert_display_from_roundtrip(&CommentId::generate());
        assert_display_from_roundtrip(&CheckpointId::generate());
    }

    #[test]
    fn ulids_are_unique_and_time_ordered() {
        let mut ids = Vec::new();
        for _ in 0..8 {
            ids.push(WorkspaceId::generate());
            std::thread::sleep(Duration::from_millis(2));
        }
        let mut sorted = ids.clone();
        sorted.sort();
        assert_eq!(
            ids, sorted,
            "ULIDs generated later must sort after earlier ones"
        );
        let unique = ids
            .iter()
            .cloned()
            .collect::<std::collections::HashSet<_>>();
        assert_eq!(unique.len(), ids.len());
    }

    #[test]
    fn from_preserves_arbitrary_string() {
        let id = WorkspaceId::from("not-a-ulid");
        assert_eq!(id.to_string(), "not-a-ulid");
        assert_eq!(
            WorkspaceId::from(String::from("abc")),
            WorkspaceId::from("abc")
        );
    }
}
