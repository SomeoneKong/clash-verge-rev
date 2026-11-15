use clash_verge_logging::{Type, logging};
use serde::Serialize;
use serde_yaml_ng::{Result, Value};

pub fn to_mihomo_config_string<T: Serialize>(data: &T) -> Result<String> {
    let yaml = to_core_config_string(data)?;
    Ok(quote_fake_ip_filter_wildcards(&yaml))
}

/// YAML for the core to read. mihomo reads a plain scalar that starts like a date as a
/// timestamp, which a field expecting a string rejects, so such strings are written quoted.
pub fn to_core_config_string<T: Serialize>(data: &T) -> Result<String> {
    let value = serde_yaml_ng::to_value(data)?;
    if !holds_date_like(&value) {
        return serde_yaml_ng::to_string(&value);
    }

    // The serializer picks the style of each string itself, but always quotes a string of
    // digits with a leading zero: such placeholders take the place of the strings to quote.
    let mut dates = Vec::new();
    let masked = mask_date_like(value.clone(), &mut dates);
    let yaml = unmask_date_like(&serde_yaml_ng::to_string(&masked)?, &dates);
    // A string of the document that reads like a placeholder would have been replaced too.
    if serde_yaml_ng::from_str::<Value>(&yaml).ok().as_ref() == Some(&value) {
        return Ok(yaml);
    }
    logging!(
        warn,
        Type::Config,
        "could not quote the date-like strings of the config, writing them plain"
    );
    serde_yaml_ng::to_string(&value)
}

const DATE_PLACEHOLDER: &str = "0942716530";
const DATE_PLACEHOLDER_DIGITS: usize = 10;

/// Starts with a four-digit year and a dash, which go-yaml checks before trying the
/// timestamp formats; quoting a string that would not parse as one is harmless.
fn is_date_like(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() > 4
        && bytes[..4].iter().all(u8::is_ascii_digit)
        && bytes[4] == b'-'
        && !value.chars().any(char::is_control)
}

fn holds_date_like(value: &Value) -> bool {
    match value {
        Value::String(string) => is_date_like(string),
        Value::Sequence(items) => items.iter().any(holds_date_like),
        Value::Mapping(map) => map
            .iter()
            .any(|(key, item)| holds_date_like(key) || holds_date_like(item)),
        Value::Tagged(tagged) => holds_date_like(&tagged.value),
        Value::Null | Value::Bool(_) | Value::Number(_) => false,
    }
}

fn mask_date_like(value: Value, dates: &mut Vec<String>) -> Value {
    match value {
        Value::String(string) if is_date_like(&string) => {
            let placeholder = format!("{DATE_PLACEHOLDER}{:0DATE_PLACEHOLDER_DIGITS$}", dates.len());
            dates.push(string);
            Value::String(placeholder)
        }
        Value::Sequence(items) => Value::Sequence(items.into_iter().map(|item| mask_date_like(item, dates)).collect()),
        Value::Mapping(map) => Value::Mapping(
            map.into_iter()
                .map(|(key, item)| (mask_date_like(key, dates), mask_date_like(item, dates)))
                .collect(),
        ),
        Value::Tagged(mut tagged) => {
            tagged.value = mask_date_like(tagged.value, dates);
            Value::Tagged(tagged)
        }
        other => other,
    }
}

fn unmask_date_like(yaml: &str, dates: &[String]) -> String {
    let marker = format!("'{DATE_PLACEHOLDER}");
    let mut result = String::with_capacity(yaml.len());
    let mut rest = yaml;
    while let Some(at) = rest.find(&marker) {
        result.push_str(&rest[..at]);
        let after = &rest[at + marker.len()..];
        let date = after
            .get(..DATE_PLACEHOLDER_DIGITS + 1)
            .and_then(|digits| digits.strip_suffix('\''))
            .and_then(|digits| digits.parse::<usize>().ok())
            .and_then(|index| dates.get(index));
        match date {
            Some(date) => {
                result.push_str(&quote_string(date));
                rest = &after[DATE_PLACEHOLDER_DIGITS + 1..];
            }
            None => {
                result.push_str(&marker);
                rest = after;
            }
        }
    }
    result.push_str(rest);
    result
}

fn quote_fake_ip_filter_wildcards(yaml: &str) -> String {
    let mut result = String::with_capacity(yaml.len());
    let mut in_fake_ip_filter = false;
    let mut fake_ip_filter_indent = 0;

    for line in yaml.lines() {
        let indent = leading_spaces(line);
        let trimmed = &line[indent..];

        if in_fake_ip_filter && indent <= fake_ip_filter_indent && !trimmed.starts_with("- ") {
            in_fake_ip_filter = false;
        }

        if !in_fake_ip_filter && trimmed == "fake-ip-filter:" {
            in_fake_ip_filter = true;
            fake_ip_filter_indent = indent;
            result.push_str(line);
            result.push('\n');
            continue;
        }

        if in_fake_ip_filter && indent >= fake_ip_filter_indent && trimmed.starts_with("- ") {
            result.push_str(&line[..indent]);
            result.push_str("- ");
            result.push_str(&quote_wildcard_item(&trimmed[2..]));
            result.push('\n');
            continue;
        }

        result.push_str(line);
        result.push('\n');
    }

    result
}

fn leading_spaces(line: &str) -> usize {
    line.len() - line.trim_start_matches(' ').len()
}

fn quote_wildcard_item(item: &str) -> String {
    if !item.starts_with('\"') && !item.starts_with('\'') && contains_wildcard(item) {
        quote_string(item)
    } else {
        item.to_string()
    }
}

fn quote_string(s: &str) -> String {
    "\'".to_string() + s.replace('\'', "''").as_str() + "\'"
}

fn contains_wildcard(value: &str) -> bool {
    value.contains('*') || value.starts_with('+') || value.starts_with('.')
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::{to_core_config_string, to_mihomo_config_string};

    fn roundtrip(input: &str) {
        let value: serde_yaml_ng::Value = serde_yaml_ng::from_str(input).expect("input yaml should parse");
        let output = to_mihomo_config_string(&value).expect("yaml should serialize");
        let reparsed: serde_yaml_ng::Value = serde_yaml_ng::from_str(&output).expect("serialized yaml should parse");

        assert_eq!(reparsed, value);
    }

    #[test]
    fn quotes_fake_ip_filter_wildcards_without_changing_values() {
        let input = r#"
dns:
  fake-ip-filter:
    - "*.lan"
    - "+.market.xiaomi.com"
    - ".example.com"
    - "time.*.com"
    - "plain.example.com"
"#;

        roundtrip(input);

        let value: serde_yaml_ng::Value = serde_yaml_ng::from_str(input).expect("input yaml should parse");
        let output = to_mihomo_config_string(&value).expect("yaml should serialize");

        assert!(output.contains("- '*.lan'"));
        assert!(output.contains("- '+.market.xiaomi.com'"));
        assert!(output.contains("- '.example.com'"));
        assert!(output.contains("- 'time.*.com'"));
        assert!(output.contains("- plain.example.com"));
    }

    #[test]
    fn quotes_strings_the_core_reads_as_dates() {
        let input = r#"
proxies:
  - name: "2026-9-14"
    password: "2026-9-14"
    headers:
      "2026-9-15": "2026-9-16 10:00:00"
    note: "2026 is a year"
    times:
      - "2026-01-01T00:00:00Z"
      - 2026
"#;

        roundtrip(input);

        let value: serde_yaml_ng::Value = serde_yaml_ng::from_str(input).expect("input yaml should parse");
        let output = to_core_config_string(&value).expect("yaml should serialize");

        assert!(output.contains("- name: '2026-9-14'"));
        assert!(output.contains("password: '2026-9-14'"));
        assert!(output.contains("'2026-9-15': '2026-9-16 10:00:00'"));
        assert!(output.contains("note: 2026 is a year"));
        assert!(output.contains("- '2026-01-01T00:00:00Z'"));
        assert!(output.contains("- 2026\n"));
    }

    #[test]
    fn a_string_that_reads_like_a_placeholder_keeps_its_value() {
        let input = r#"
password: "2026-9-14"
other: "09427165300000000000"
"#;
        let value: serde_yaml_ng::Value = serde_yaml_ng::from_str(input).expect("input yaml should parse");
        let output = to_core_config_string(&value).expect("yaml should serialize");
        let reparsed: serde_yaml_ng::Value = serde_yaml_ng::from_str(&output).expect("serialized yaml should parse");

        assert_eq!(reparsed, value);
    }

    #[test]
    fn nested_multiline_strings_roundtrip() {
        roundtrip(
            r"
items:
  - name: demo
    desc: |
      line1
      line2
",
        );
    }
}
