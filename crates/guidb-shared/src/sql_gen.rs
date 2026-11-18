use crate::{CellValue, EngineKind, StagedMutation};

/// Quote an SQL identifier safely according to engine dialect.
pub fn quote_ident(ident: &str, engine: EngineKind) -> String {
    match engine {
        EngineKind::Mysql => format!("`{}`", ident.replace('`', "``")),
        _ => format!("\"{}\"", ident.replace('"', "\"\"")),
    }
}

/// Validate a bare SQL identifier (table, column, index) for DDL builders.
pub fn validate_identifier(ident: &str) -> Result<String, String> {
    let trimmed = ident.trim();
    if trimmed.is_empty() {
        return Err("Identifier cannot be empty".to_string());
    }
    if trimmed.len() > 128 {
        return Err("Identifier exceeds 128 characters".to_string());
    }
    let first = trimmed.chars().next().ok_or("Empty identifier")?;
    if !(first.is_ascii_alphabetic() || first == '_') {
        return Err(format!(
            "Identifier '{trimmed}' must start with a letter or '_'"
        ));
    }
    if !trimmed
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        return Err(format!(
            "Identifier '{trimmed}' contains invalid characters; only letters, digits, and '_' are allowed"
        ));
    }
    Ok(trimmed.to_string())
}

/// Format a CellValue as a safe SQL literal for preview & DDL display.
pub fn format_sql_literal(val: &CellValue) -> String {
    match val {
        CellValue::Null => "NULL".to_string(),
        CellValue::Bool(b) => if *b { "TRUE" } else { "FALSE" }.to_string(),
        CellValue::Int(i) => i.to_string(),
        CellValue::Float(f) => {
            if f.is_finite() {
                format!("{f}")
            } else {
                "NULL".to_string()
            }
        }
        CellValue::Text(s) => format!("'{}'", s.replace('\'', "''")),
        CellValue::Json(v) => {
            let s = serde_json::to_string(v).unwrap_or_else(|_| "{}".to_string());
            format!("'{}'", s.replace('\'', "''"))
        }
        CellValue::Bytes(b) => format!("'{}'", b.replace('\'', "''")),
    }
}

/// Generate deterministic human-readable SQL or NoSQL/KV commands for staged mutations.
pub fn preview_mutations(mutations: &[StagedMutation], engine: EngineKind) -> Vec<String> {
    let mut out = Vec::with_capacity(mutations.len());
    for m in mutations {
        match engine {
            EngineKind::MongoDb => match m {
                StagedMutation::Insert { item, values } => {
                    let doc = serde_json::to_string(
                        &values
                            .iter()
                            .map(|(k, v)| (k.clone(), cell_to_json(v)))
                            .collect::<serde_json::Map<String, serde_json::Value>>(),
                    )
                    .unwrap_or_else(|_| "{}".to_string());
                    out.push(format!("db.{item}.insertOne({doc});"));
                }
                StagedMutation::Update {
                    item,
                    pk_match,
                    changes,
                } => {
                    let filter = map_to_json_string(pk_match);
                    let set_doc = map_to_json_string(changes);
                    out.push(format!(
                        "db.{item}.updateOne({filter}, {{\"$set\": {set_doc}}});"
                    ));
                }
                StagedMutation::Delete { item, pk_match } => {
                    let filter = map_to_json_string(pk_match);
                    out.push(format!("db.{item}.deleteOne({filter});"));
                }
            },
            EngineKind::Redis => match m {
                StagedMutation::Insert { item: _, values } => {
                    let key = values
                        .get("key")
                        .map(|v| v.display_text())
                        .unwrap_or_else(|| "new_key".to_string());
                    let val = values
                        .get("value")
                        .map(|v| v.display_text())
                        .unwrap_or_default();
                    let ttl = values
                        .get("ttl_seconds")
                        .map(|v| v.display_text())
                        .unwrap_or_else(|| "-1".to_string());
                    if ttl != "-1" {
                        out.push(format!("SET {key} \"{val}\" EX {ttl}"));
                    } else {
                        out.push(format!("SET {key} \"{val}\""));
                    }
                }
                StagedMutation::Update {
                    item: _,
                    pk_match,
                    changes,
                } => {
                    let key = pk_match
                        .get("key")
                        .map(|v| v.display_text())
                        .unwrap_or_default();
                    if let Some(val) = changes.get("value") {
                        out.push(format!("SET {key} \"{}\"", val.display_text()));
                    }
                    if let Some(ttl) = changes.get("ttl_seconds") {
                        out.push(format!("EXPIRE {key} {}", ttl.display_text()));
                    }
                }
                StagedMutation::Delete { item: _, pk_match } => {
                    let key = pk_match
                        .get("key")
                        .map(|v| v.display_text())
                        .unwrap_or_default();
                    out.push(format!("DEL {key}"));
                }
            },
            _ => match m {
                StagedMutation::Insert { item, values } => {
                    let cols: Vec<String> = values.keys().map(|c| quote_ident(c, engine)).collect();
                    let vals: Vec<String> = values.values().map(format_sql_literal).collect();
                    out.push(format!(
                        "INSERT INTO {} ({}) VALUES ({});",
                        quote_ident(item, engine),
                        cols.join(", "),
                        vals.join(", ")
                    ));
                }
                StagedMutation::Update {
                    item,
                    pk_match,
                    changes,
                } => {
                    let sets: Vec<String> = changes
                        .iter()
                        .map(|(k, v)| {
                            format!("{} = {}", quote_ident(k, engine), format_sql_literal(v))
                        })
                        .collect();
                    let wheres: Vec<String> = pk_match
                        .iter()
                        .map(|(k, v)| match v {
                            CellValue::Null => format!("{} IS NULL", quote_ident(k, engine)),
                            _ => format!("{} = {}", quote_ident(k, engine), format_sql_literal(v)),
                        })
                        .collect();
                    out.push(format!(
                        "UPDATE {} SET {} WHERE {};",
                        quote_ident(item, engine),
                        sets.join(", "),
                        wheres.join(" AND ")
                    ));
                }
                StagedMutation::Delete { item, pk_match } => {
                    let wheres: Vec<String> = pk_match
                        .iter()
                        .map(|(k, v)| match v {
                            CellValue::Null => format!("{} IS NULL", quote_ident(k, engine)),
                            _ => format!("{} = {}", quote_ident(k, engine), format_sql_literal(v)),
                        })
                        .collect();
                    out.push(format!(
                        "DELETE FROM {} WHERE {};",
                        quote_ident(item, engine),
                        wheres.join(" AND ")
                    ));
                }
            },
        }
    }
    out
}

fn cell_to_json(val: &CellValue) -> serde_json::Value {
    match val {
        CellValue::Null => serde_json::Value::Null,
        CellValue::Bool(b) => serde_json::Value::Bool(*b),
        CellValue::Int(i) => serde_json::json!(i),
        CellValue::Float(f) => serde_json::json!(f),
        CellValue::Text(s) => serde_json::Value::String(s.clone()),
        CellValue::Json(v) => v.clone(),
        CellValue::Bytes(b) => serde_json::Value::String(b.clone()),
    }
}

fn map_to_json_string(map: &std::collections::BTreeMap<String, CellValue>) -> String {
    let obj: serde_json::Map<String, serde_json::Value> = map
        .iter()
        .map(|(k, v)| (k.clone(), cell_to_json(v)))
        .collect();
    serde_json::to_string(&obj).unwrap_or_else(|_| "{}".to_string())
}

/// Format an SQL query cleanly with consistent keyword line breaks while preserving string literals.
pub fn format_sql(input: &str) -> String {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    if trimmed.starts_with('{') || trimmed.starts_with('[') {
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(trimmed) {
            if let Ok(pretty) = serde_json::to_string_pretty(&val) {
                return pretty;
            }
        }
    }

    let major_clauses = [
        "SELECT",
        "FROM",
        "LEFT JOIN",
        "RIGHT JOIN",
        "INNER JOIN",
        "CROSS JOIN",
        "JOIN",
        "WHERE",
        "GROUP BY",
        "HAVING",
        "ORDER BY",
        "LIMIT",
        "OFFSET",
        "VALUES",
        "SET",
    ];
    let keywords = [
        "SELECT",
        "FROM",
        "WHERE",
        "AND",
        "OR",
        "AS",
        "ON",
        "IN",
        "IS",
        "NOT",
        "NULL",
        "JOIN",
        "LEFT",
        "RIGHT",
        "INNER",
        "CROSS",
        "GROUP",
        "BY",
        "HAVING",
        "ORDER",
        "ASC",
        "DESC",
        "LIMIT",
        "OFFSET",
        "INSERT",
        "INTO",
        "VALUES",
        "UPDATE",
        "SET",
        "DELETE",
        "CREATE",
        "TABLE",
        "ALTER",
        "DROP",
        "INDEX",
        "UNIQUE",
        "PRIMARY",
        "KEY",
        "FOREIGN",
        "REFERENCES",
        "EXPLAIN",
        "QUERY",
        "PLAN",
        "COUNT",
        "SUM",
        "AVG",
        "MIN",
        "MAX",
    ];

    let tokens = tokenize_sql(trimmed);
    let mut result = String::new();
    let mut i = 0;
    while i < tokens.len() {
        let tok = &tokens[i];
        if tok.is_literal {
            if !result.is_empty() && !result.ends_with(['\n', '(', ' ']) {
                result.push(' ');
            }
            result.push_str(&tok.text);
            i += 1;
            continue;
        }

        let upper = tok.text.to_ascii_uppercase();
        let next_upper = tokens
            .get(i + 1)
            .filter(|t| !t.is_literal)
            .map(|t| t.text.to_ascii_uppercase());

        let two_word = next_upper
            .as_ref()
            .map(|n| format!("{upper} {n}"))
            .unwrap_or_default();

        if major_clauses.contains(&two_word.as_str()) {
            if !result.is_empty() && !result.ends_with('\n') {
                result.push('\n');
            }
            result.push_str(&two_word);
            i += 2;
            continue;
        }

        if major_clauses.contains(&upper.as_str()) {
            if !result.is_empty() && !result.ends_with('\n') {
                result.push('\n');
            }
            result.push_str(&upper);
            i += 1;
            continue;
        }

        if tok.text == "," {
            result.push(',');
            result.push('\n');
            result.push_str("  ");
            i += 1;
            continue;
        }
        if tok.text == ";" {
            result.push(';');
            if i + 1 < tokens.len() {
                result.push('\n');
            }
            i += 1;
            continue;
        }
        if tok.text == "(" {
            if !result.is_empty() && !result.ends_with(['\n', '(', ' ']) {
                result.push(' ');
            }
            result.push('(');
            i += 1;
            continue;
        }
        if tok.text == ")" {
            result.push(')');
            i += 1;
            continue;
        }

        if !result.is_empty() && !result.ends_with(['\n', '(', ' ']) {
            result.push(' ');
        }
        if keywords.contains(&upper.as_str()) {
            result.push_str(&upper);
        } else {
            result.push_str(&tok.text);
        }
        i += 1;
    }

    result.trim().to_string()
}

struct SqlToken {
    text: String,
    is_literal: bool,
}

fn tokenize_sql(input: &str) -> Vec<SqlToken> {
    let mut tokens = Vec::new();
    let chars: Vec<char> = input.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let ch = chars[i];
        if ch.is_whitespace() {
            i += 1;
            continue;
        }
        if ch == '\'' || ch == '"' || ch == '`' {
            let quote = ch;
            let mut s = String::from(quote);
            i += 1;
            while i < chars.len() {
                s.push(chars[i]);
                if chars[i] == quote {
                    if i + 1 < chars.len() && chars[i + 1] == quote {
                        i += 1;
                        s.push(chars[i]);
                    } else {
                        i += 1;
                        break;
                    }
                }
                i += 1;
            }
            tokens.push(SqlToken {
                text: s,
                is_literal: true,
            });
            continue;
        }
        if matches!(ch, ',' | ';' | '(' | ')') {
            tokens.push(SqlToken {
                text: ch.to_string(),
                is_literal: false,
            });
            i += 1;
            continue;
        }
        let mut word = String::new();
        while i < chars.len()
            && !chars[i].is_whitespace()
            && !matches!(chars[i], ',' | ';' | '(' | ')' | '\'' | '"' | '`')
        {
            word.push(chars[i]);
            i += 1;
        }
        tokens.push(SqlToken {
            text: word,
            is_literal: false,
        });
    }
    tokens
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn preview_generates_sql_and_nosql() {
        let mut pk = BTreeMap::new();
        pk.insert("id".to_string(), CellValue::Int(42));
        let mut changes = BTreeMap::new();
        changes.insert("name".to_string(), CellValue::Text("O'Brien".to_string()));
        let m = vec![StagedMutation::Update {
            item: "customers".to_string(),
            pk_match: pk,
            changes,
        }];
        let sql = preview_mutations(&m, EngineKind::Postgres);
        assert_eq!(
            sql[0],
            "UPDATE \"customers\" SET \"name\" = 'O''Brien' WHERE \"id\" = 42;"
        );

        let mongo = preview_mutations(&m, EngineKind::MongoDb);
        assert!(mongo[0].starts_with("db.customers.updateOne("));
    }

    #[test]
    fn format_sql_preserves_literals_and_breaks_clauses() {
        let formatted = format_sql(
            "select id, name from customers where status = 'active' order by id desc limit 25;",
        );
        assert!(formatted.contains("SELECT"));
        assert!(formatted
            .contains("\nFROM customers\nWHERE status = 'active'\nORDER BY id DESC\nLIMIT 25;"));
    }

    #[test]
    fn validate_identifier_rejects_injection() {
        assert!(validate_identifier("orders_2026").is_ok());
        assert!(validate_identifier("orders; DROP TABLE x").is_err());
        assert!(validate_identifier("1bad").is_err());
    }
}
