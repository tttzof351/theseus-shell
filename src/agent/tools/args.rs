use std::io;

use serde_json::Value;

pub(super) fn parse_arguments(arguments: &str) -> io::Result<Value> {
    serde_json::from_str(arguments)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err.to_string()))
}

pub(super) fn string_arg<'a>(arguments: &'a Value, key: &str) -> io::Result<&'a str> {
    arguments
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, format!("missing `{key}`")))
}

pub(super) fn optional_usize_arg(arguments: &Value, key: &str) -> io::Result<Option<usize>> {
    let Some(value) = arguments.get(key) else {
        return Ok(None);
    };

    let Some(value) = value.as_u64() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("`{key}` must be a non-negative integer"),
        ));
    };

    usize::try_from(value).map(Some).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("`{key}` is too large for this platform"),
        )
    })
}
