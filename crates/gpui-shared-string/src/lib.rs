use std::{
    borrow::{Borrow, Cow},
    fmt,
    hash::{Hash, Hasher},
    ops::Deref,
    sync::Arc,
};

use schemars::{JsonSchema, Schema, SchemaGenerator};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

#[derive(Clone)]
enum Storage {
    Static(&'static str),
    Shared(Arc<str>),
}

#[derive(Clone)]
pub struct SharedString(Storage);

impl SharedString {
    #[must_use]
    pub fn new(value: impl AsRef<str>) -> Self {
        Self(Storage::Shared(Arc::from(value.as_ref())))
    }

    #[must_use]
    pub const fn new_static(value: &'static str) -> Self {
        Self(Storage::Static(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        match &self.0 {
            Storage::Static(value) => value,
            Storage::Shared(value) => value,
        }
    }
}

impl Default for SharedString {
    fn default() -> Self {
        Self::new_static("")
    }
}

impl PartialEq for SharedString {
    fn eq(&self, other: &Self) -> bool {
        self.as_str() == other.as_str()
    }
}

impl Eq for SharedString {}

impl PartialOrd for SharedString {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for SharedString {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.as_str().cmp(other.as_str())
    }
}

impl Hash for SharedString {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.as_str().hash(state);
    }
}

impl AsRef<str> for SharedString {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl Borrow<str> for SharedString {
    fn borrow(&self) -> &str {
        self.as_str()
    }
}

impl Deref for SharedString {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        self.as_str()
    }
}

impl fmt::Debug for SharedString {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.as_str().fmt(formatter)
    }
}

impl fmt::Display for SharedString {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.as_str().fmt(formatter)
    }
}

impl From<&str> for SharedString {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

impl From<String> for SharedString {
    fn from(value: String) -> Self {
        Self(Storage::Shared(Arc::from(value)))
    }
}

impl From<Arc<str>> for SharedString {
    fn from(value: Arc<str>) -> Self {
        Self(Storage::Shared(value))
    }
}

impl From<Box<str>> for SharedString {
    fn from(value: Box<str>) -> Self {
        Self(Storage::Shared(Arc::from(value)))
    }
}

impl<'a> From<Cow<'a, str>> for SharedString {
    fn from(value: Cow<'a, str>) -> Self {
        Self::new(value)
    }
}

impl From<SharedString> for String {
    fn from(value: SharedString) -> Self {
        value.as_str().to_owned()
    }
}

impl From<&SharedString> for SharedString {
    fn from(value: &SharedString) -> Self {
        value.clone()
    }
}

impl PartialEq<str> for SharedString {
    fn eq(&self, other: &str) -> bool {
        self.as_ref() == other
    }
}

impl PartialEq<&str> for SharedString {
    fn eq(&self, other: &&str) -> bool {
        self.as_ref() == *other
    }
}

impl PartialEq<String> for SharedString {
    fn eq(&self, other: &String) -> bool {
        self.as_ref() == other
    }
}

impl Serialize for SharedString {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self)
    }
}

impl<'de> Deserialize<'de> for SharedString {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer).map(Self::from)
    }
}

impl JsonSchema for SharedString {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("SharedString")
    }

    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        String::json_schema(generator)
    }
}

#[cfg(test)]
mod tests {
    use super::SharedString;
    use std::collections::HashMap;

    #[test]
    fn preserves_text_and_value_semantics() {
        let value = SharedString::from("hello");
        let clone = value.clone();
        let mut values = HashMap::new();
        values.insert(value, 1);

        assert_eq!(clone.as_ref(), "hello");
        assert_eq!(values.get("hello"), Some(&1));
    }
}
