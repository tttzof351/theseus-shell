use super::{Budget, StreamError};
use serde_json::{Map, Value};
use std::collections::HashMap;

#[derive(Default)]
pub(super) struct Details {
    entries: Vec<Map<String, Value>>,
    indices: HashMap<u64, usize>,
    ids: HashMap<String, usize>,
    displayed_bytes: Vec<usize>,
}

impl Details {
    #[cfg(test)]
    pub fn append(
        &mut self,
        fragments: Vec<Value>,
        budget: &mut Budget,
    ) -> Result<String, StreamError> {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(self.append_async(fragments, budget))
    }

    pub async fn append_async(
        &mut self,
        fragments: Vec<Value>,
        budget: &mut Budget,
    ) -> Result<String, StreamError> {
        let mut display = String::new();
        for (position, fragment) in fragments.into_iter().enumerate() {
            if position % 64 == 0 {
                tokio::task::yield_now().await;
            }
            let Value::Object(fragment) = fragment else {
                return Err(StreamError::protocol(
                    "reasoning_details entry is not an object",
                ));
            };
            let index = match fragment.get("index").filter(|v| !v.is_null()) {
                Some(value) => Some(
                    value
                        .as_u64()
                        .ok_or_else(|| StreamError::protocol("Invalid reasoning index"))?,
                ),
                None => None,
            };
            let id = match fragment.get("id").filter(|v| !v.is_null()) {
                Some(value) => Some(
                    value
                        .as_str()
                        .ok_or_else(|| StreamError::protocol("Invalid reasoning id"))?
                        .to_owned(),
                ),
                None => None,
            };
            let by_index = index.and_then(|i| self.indices.get(&i)).copied();
            let by_id = id.as_ref().and_then(|i| self.ids.get(i)).copied();
            if matches!((by_index, by_id), (Some(a), Some(b)) if a != b) {
                return Err(StreamError::protocol("Conflicting reasoning identities"));
            }
            let position = by_index.or(by_id).unwrap_or(self.entries.len());
            if position == self.entries.len() {
                budget.reserve(2)?;
                self.entries.push(Map::new());
                self.displayed_bytes.push(0);
            }
            let entry = &mut self.entries[position];
            for (key, value) in fragment {
                // Preserve explicit nulls in opaque provider metadata, but a
                // later real value may fill a previously null field.
                if value.is_null() && entry.contains_key(&key) {
                    continue;
                }
                let old = entry.get_mut(&key).filter(|v| !v.is_null());
                if let Some(old) = old {
                    if matches!(key.as_str(), "text" | "summary" | "data" | "signature") {
                        let text = value
                            .as_str()
                            .ok_or_else(|| StreamError::protocol("Invalid reasoning fragment"))?;
                        let Value::String(accumulated) = old else {
                            return Err(StreamError::protocol(
                                "Conflicting reasoning fragment types",
                            ));
                        };
                        budget.reserve(text.len())?;
                        accumulated.push_str(text);
                    } else if *old != value {
                        return Err(StreamError::protocol(format!(
                            "Conflicting reasoning field {key}"
                        )));
                    }
                } else {
                    budget.reserve(key.len() + value.to_string().len())?;
                    entry.insert(key, value);
                }
            }
            // A provider may identify the detail's type after its first text.
            // Once the type is known, expose the buffered prefix exactly once.
            let visible_field = match entry.get("type").and_then(Value::as_str) {
                Some("reasoning.text") => Some("text"),
                Some("reasoning.summary") => Some("summary"),
                _ => None,
            };
            if let Some(text) = visible_field
                .and_then(|field| entry.get(field))
                .and_then(Value::as_str)
            {
                display.push_str(&text[self.displayed_bytes[position]..]);
                self.displayed_bytes[position] = text.len();
            }
            if let Some(index) = index {
                self.indices.insert(index, position);
            }
            if let Some(id) = id {
                self.ids.insert(id, position);
            }
        }
        Ok(display)
    }

    pub fn into_values(self) -> Option<Vec<Value>> {
        (!self.entries.is_empty()).then(|| self.entries.into_iter().map(Value::Object).collect())
    }
}
