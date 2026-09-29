//! Live Cordis objects inside generated data types.
//!
//! Generated types keep using serde. While a value is decoded, every object
//! reference in it is replaced by a marker that `ObjectRef` deserializes back
//! into the reference; encoding an argument does the reverse. Outside these
//! two operations an `ObjectRef` cannot be serialized.

use std::cell::RefCell;

use serde::{de, ser, Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Value as Json};

use crate::rpc::{self, Reference, Value};
use crate::Error;

const MARK: &str = "\u{0}rutis:reference";

thread_local! {
    static DECODING: RefCell<Vec<Reference>> = const { RefCell::new(Vec::new()) };
    static ENCODING: RefCell<Option<Vec<Reference>>> = const { RefCell::new(None) };
}

/// A live object owned by the Cordis side: property reads and method calls
/// go to that object, so its identity and state are never copied. Passing it
/// back as an argument hands the original object to Cordis.
/// Two `ObjectRef`s are equal when they address the same object.
#[derive(Clone, Debug, PartialEq)]
pub struct ObjectRef(Reference);

impl ObjectRef {
    /// Call a method and wait for it to return.
    pub fn call(&self, method: &str, args: Vec<Value>) -> Result<Value, Error> {
        self.0.call_method(method, Value::List(args))
    }

    /// Call a method; a returned Promise is awaited.
    pub async fn call_async(&self, method: &str, args: Vec<Value>) -> Result<Value, Error> {
        rpc::settle(self.0.call_method_async(method, Value::List(args)).await?).await
    }

    /// Read a property. Every read reaches the object.
    pub fn get(&self, property: &str) -> Result<Value, Error> {
        self.0.get(property)
    }

    pub fn reference(&self) -> &Reference {
        &self.0
    }
}

impl<'de> Deserialize<'de> for ObjectRef {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let marker = Json::deserialize(deserializer)?;
        let index = marker
            .as_object()
            .filter(|fields| fields.len() == 1)
            .and_then(|fields| fields.get(MARK))
            .and_then(Json::as_u64)
            .ok_or_else(|| de::Error::custom("expected a live object, received data"))?;
        let reference = DECODING
            .with(|references| references.borrow().get(index as usize).cloned())
            .ok_or_else(|| de::Error::custom("object reference outside of a decode"))?;
        if !reference.is_object() {
            return Err(de::Error::custom(
                "expected a live object, received a function",
            ));
        }
        Ok(Self(reference))
    }
}

impl Serialize for ObjectRef {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let index = ENCODING.with(|references| {
            references.borrow_mut().as_mut().map(|references| {
                references.push(self.0.clone());
                references.len() - 1
            })
        });
        let index = index.ok_or_else(|| {
            ser::Error::custom("a live object can only be serialized as a call argument")
        })?;
        let mut marker = Map::new();
        marker.insert(MARK.into(), index.into());
        Json::Object(marker).serialize(serializer)
    }
}

/// Decode a call result into a generated type; object references in it
/// become `ObjectRef`s.
pub fn decode_value<T: de::DeserializeOwned>(value: Value) -> Result<T, Error> {
    let mut references = Vec::new();
    let json = marked(value, &mut references);
    let previous = DECODING.with(|current| current.replace(references));
    let result = serde_json::from_value(json);
    DECODING.with(|current| current.replace(previous));
    result.map_err(|error| Error::Value(error.to_string()))
}

fn marked(value: Value, references: &mut Vec<Reference>) -> Json {
    match value {
        Value::Undefined => Json::Null,
        Value::Data(json) => json,
        Value::List(values) => Json::Array(
            values
                .into_iter()
                .map(|value| marked(value, references))
                .collect(),
        ),
        Value::Record(fields) => Json::Object(
            fields
                .into_iter()
                .map(|(key, value)| (key, marked(value, references)))
                .collect(),
        ),
        Value::Signal => Json::Null,
        Value::Reference(reference) => {
            references.push(reference);
            let mut marker = Map::new();
            marker.insert(MARK.into(), (references.len() - 1).into());
            Json::Object(marker)
        }
    }
}

/// Encode a call argument; `ObjectRef`s in it pass their original object.
pub fn arg<T: Serialize + ?Sized>(value: &T) -> Result<Value, Error> {
    let previous = ENCODING.with(|current| current.replace(Some(Vec::new())));
    let json = serde_json::to_value(value);
    let references = ENCODING
        .with(|current| current.replace(previous))
        .unwrap_or_default();
    let json = json.map_err(|error| Error::Value(error.to_string()))?;
    Ok(if references.is_empty() {
        Value::Data(json)
    } else {
        unmarked(json, &references)
    })
}

fn unmarked(json: Json, references: &[Reference]) -> Value {
    match json {
        Json::Object(fields) => {
            if fields.len() == 1 {
                if let Some(reference) = fields
                    .get(MARK)
                    .and_then(Json::as_u64)
                    .and_then(|index| references.get(index as usize))
                {
                    return Value::Reference(reference.clone());
                }
            }
            Value::Record(
                fields
                    .into_iter()
                    .map(|(key, value)| (key, unmarked(value, references)))
                    .collect(),
            )
        }
        Json::Array(values) => Value::List(
            values
                .into_iter()
                .map(|value| unmarked(value, references))
                .collect(),
        ),
        json => Value::Data(json),
    }
}
