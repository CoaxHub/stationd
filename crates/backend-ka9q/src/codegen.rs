// SPDX-License-Identifier: GPL-3.0-only
//! Build-time generator for the ka9q-radio tag enums.
//!
//! `build.rs` includes this file with `#[path = "src/codegen.rs"] mod codegen;`
//! and the library compiles it only under `#[cfg(test)]` so the parser is
//! unit-tested. It turns the C enums of the pinned `status.h` and `rtp.h` into
//! Rust enums (see [`generate_all`]).
//!
//! The parser accepts the small C subset used by those enums: identifiers,
//! decimal/hex/octal integer literals with an optional sign, `//` and
//! `/* */` comments anywhere, blank lines and trailing commas. Anything else
//! inside an enum body (expressions, references to other enumerators,
//! attributes) is rejected with a clear error so a surprising upstream change
//! fails the build instead of producing wrong tag numbers.

use std::fmt::Write as _;

/// One enumerator of a parsed C enum.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CVariant {
    /// The C identifier, e.g. `RADIO_FREQUENCY`.
    pub name: String,
    /// The value after applying explicit values and implicit increments.
    pub value: i64,
}

/// A parsed C `enum name { ... };` definition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CEnum {
    /// The enum tag, e.g. `status_type`.
    pub name: String,
    /// The enumerators in source order.
    pub variants: Vec<CVariant>,
}

/// The Rust representation of a generated enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Repr {
    /// `#[repr(u8)]`, with `from_u8`. Every value must be in `0..=255`.
    U8,
    /// `#[repr(i32)]`, with `from_i32`.
    I32,
}

/// How to turn one C enum into a Rust enum.
#[derive(Debug, Clone, Copy)]
pub struct EnumSpec {
    /// The C enum tag to look for, e.g. `status_type`.
    pub c_name: &'static str,
    /// The Rust type name, e.g. `StatusType`.
    pub rust_name: &'static str,
    /// The Rust representation.
    pub repr: Repr,
    /// C enumerators to leave out (sentinels such as `N_DEMOD`).
    pub skip: &'static [&'static str],
    /// A suffix removed from every C name before conversion (`_DEMOD`).
    pub strip_suffix: Option<&'static str>,
    /// The doc comment of the generated type.
    pub doc: &'static str,
}

/// The enums generated from the pinned headers: (header file name, spec).
pub const SPECS: &[(&str, EnumSpec)] = &[
    (
        "status.h",
        EnumSpec {
            c_name: "pkt_type",
            rust_name: "PktType",
            repr: Repr::U8,
            skip: &[],
            strip_suffix: None,
            doc: "Packet type, the first byte of every command/status packet (`enum pkt_type`).",
        },
    ),
    (
        "status.h",
        EnumSpec {
            c_name: "status_type",
            rust_name: "StatusType",
            repr: Repr::U8,
            skip: &[],
            strip_suffix: None,
            doc: "TLV tag of a command or status field (`enum status_type`).",
        },
    ),
    (
        "status.h",
        EnumSpec {
            c_name: "demod_type",
            rust_name: "DemodType",
            repr: Repr::I32,
            skip: &["INVALID_DEMOD", "N_DEMOD"],
            strip_suffix: Some("_DEMOD"),
            doc: "Demodulator type of a channel (`enum demod_type`, sentinels removed).",
        },
    ),
    (
        "rtp.h",
        EnumSpec {
            c_name: "encoding",
            rust_name: "Encoding",
            repr: Repr::I32,
            skip: &["UNUSED_ENCODING"],
            strip_suffix: None,
            doc: "Output sample encoding of a channel (`enum encoding` in `rtp.h`, sentinel removed).",
        },
    ),
];

/// Removes `//` and `/* */` comments, keeping string and character literals
/// intact and replacing each comment with a space (or its newlines) so tokens
/// on either side stay separate.
pub fn strip_comments(src: &str) -> Result<String, String> {
    let mut out = String::with_capacity(src.len());
    let mut chars = src.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '/' if chars.peek() == Some(&'/') => {
                for c in chars.by_ref() {
                    if c == '\n' {
                        out.push('\n');
                        break;
                    }
                }
            }
            '/' if chars.peek() == Some(&'*') => {
                chars.next();
                let mut closed = false;
                let mut prev = '\0';
                for c in chars.by_ref() {
                    if prev == '*' && c == '/' {
                        closed = true;
                        break;
                    }
                    if c == '\n' {
                        out.push('\n');
                    }
                    prev = c;
                }
                if !closed {
                    return Err("unterminated /* comment".to_owned());
                }
                out.push(' ');
            }
            '"' | '\'' => {
                out.push(c);
                let mut escaped = false;
                let mut closed = false;
                for d in chars.by_ref() {
                    out.push(d);
                    if escaped {
                        escaped = false;
                    } else if d == '\\' {
                        escaped = true;
                    } else if d == c {
                        closed = true;
                        break;
                    }
                }
                if !closed {
                    return Err(format!("unterminated {c} literal"));
                }
            }
            _ => out.push(c),
        }
    }
    Ok(out)
}

fn is_ident(s: &str) -> bool {
    let mut chars = s.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Parses a C integer literal: optional sign, then decimal, `0x` hex or
/// leading-zero octal, with optional `u`/`l` suffixes.
pub fn parse_int(text: &str) -> Result<i64, String> {
    let t = text.trim();
    let (negative, rest) = match t.strip_prefix('-') {
        Some(r) => (true, r.trim_start()),
        None => (false, t.strip_prefix('+').unwrap_or(t).trim_start()),
    };
    let digits = rest.trim_end_matches(['u', 'U', 'l', 'L']);
    if !digits.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return Err(format!("unsupported enumerator value `{text}`"));
    }
    let parsed = if let Some(hex) = digits
        .strip_prefix("0x")
        .or_else(|| digits.strip_prefix("0X"))
    {
        i64::from_str_radix(hex, 16)
    } else if digits.len() > 1 && digits.starts_with('0') {
        i64::from_str_radix(&digits[1..], 8)
    } else {
        digits.parse::<i64>()
    };
    let magnitude = parsed.map_err(|_| format!("unsupported enumerator value `{text}`"))?;
    Ok(if negative { -magnitude } else { magnitude })
}

fn parse_body(name: &str, body: &str) -> Result<Vec<CVariant>, String> {
    let mut variants = Vec::new();
    let mut next: i64 = 0;
    for item in body.split(',') {
        let item = item.trim();
        if item.is_empty() {
            continue;
        }
        let (ident, value) = match item.split_once('=') {
            Some((ident, expr)) => (ident.trim(), parse_int(expr)?),
            None => (item, next),
        };
        if !is_ident(ident) {
            return Err(format!(
                "enum {name}: unsupported enumerator `{item}` (only `NAME` or `NAME = <integer>` are allowed)"
            ));
        }
        if variants.iter().any(|v: &CVariant| v.name == ident) {
            return Err(format!("enum {name}: duplicate enumerator {ident}"));
        }
        variants.push(CVariant {
            name: ident.to_owned(),
            value,
        });
        next = value
            .checked_add(1)
            .ok_or_else(|| format!("enum {name}: value overflow after {ident}"))?;
    }
    Ok(variants)
}

/// Finds every `enum <name> { ... }` definition in a C source. Uses of an
/// enum type (`enum status_type type;`) are ignored.
pub fn parse_enums(src: &str) -> Result<Vec<CEnum>, String> {
    let clean = strip_comments(src)?;
    let is_ident_char = |c: char| c.is_ascii_alphanumeric() || c == '_';
    let mut enums = Vec::new();
    let mut prev: Option<char> = None;
    let mut rest = clean.as_str();
    while let Some(pos) = rest.find("enum") {
        let before = rest[..pos].chars().next_back().or(prev);
        let after = &rest[pos + 4..];
        prev = Some('m');
        rest = after;
        if before.is_some_and(is_ident_char)
            || !after.starts_with(|c: char| c.is_ascii_whitespace())
        {
            continue;
        }
        let after = after.trim_start();
        let name_len = after
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .unwrap_or(after.len());
        let name = &after[..name_len];
        let tail = after[name_len..].trim_start();
        if name.is_empty() || !tail.starts_with('{') {
            continue;
        }
        let Some(close) = tail.find('}') else {
            return Err(format!("enum {name}: missing closing brace"));
        };
        let variants = parse_body(name, &tail[1..close])?;
        enums.push(CEnum {
            name: name.to_owned(),
            variants,
        });
        prev = Some('}');
        rest = &tail[close + 1..];
    }
    Ok(enums)
}

/// Converts a C `SCREAMING_SNAKE` name to Rust `CamelCase`
/// (`OPUS_BIT_RATE` becomes `OpusBitRate`, `S16LE` becomes `S16le`).
pub fn to_camel_case(c_name: &str) -> String {
    let mut out = String::with_capacity(c_name.len());
    for word in c_name.split('_').filter(|w| !w.is_empty()) {
        let mut chars = word.chars();
        if let Some(first) = chars.next() {
            out.push(first.to_ascii_uppercase());
            out.extend(chars.map(|c| c.to_ascii_lowercase()));
        }
    }
    out
}

fn doc_escape(s: &str) -> String {
    s.replace('"', "\\\"")
}

/// Generates the Rust source for one enum from its parsed C definition.
pub fn generate_enum(spec: &EnumSpec, c: &CEnum) -> Result<String, String> {
    for skipped in spec.skip {
        if !c.variants.iter().any(|v| v.name == *skipped) {
            return Err(format!(
                "enum {}: expected sentinel {skipped} is missing; update the codegen spec",
                c.name
            ));
        }
    }
    let mut rows: Vec<(String, &str, i64)> = Vec::new();
    for v in c
        .variants
        .iter()
        .filter(|v| !spec.skip.contains(&v.name.as_str()))
    {
        match spec.repr {
            Repr::U8 if !(0..=255).contains(&v.value) => {
                return Err(format!(
                    "enum {}: {} = {} does not fit in a u8 tag (0..=255)",
                    c.name, v.name, v.value
                ));
            }
            Repr::I32 if i32::try_from(v.value).is_err() => {
                return Err(format!(
                    "enum {}: {} = {} does not fit in an i32",
                    c.name, v.name, v.value
                ));
            }
            _ => {}
        }
        if let Some(dup) = rows.iter().find(|r| r.2 == v.value) {
            return Err(format!(
                "enum {}: {} and {} share the value {}",
                c.name, dup.1, v.name, v.value
            ));
        }
        let base = spec
            .strip_suffix
            .and_then(|s| v.name.strip_suffix(s))
            .unwrap_or(&v.name);
        let rust = to_camel_case(base);
        if rust.is_empty() || rust.starts_with(|ch: char| ch.is_ascii_digit()) {
            return Err(format!("enum {}: cannot name variant {}", c.name, v.name));
        }
        if let Some(dup) = rows.iter().find(|r| r.0 == rust) {
            return Err(format!(
                "enum {}: {} and {} both map to Rust name {rust}",
                c.name, dup.1, v.name
            ));
        }
        rows.push((rust, &v.name, v.value));
    }
    if rows.is_empty() {
        return Err(format!("enum {}: no variants", c.name));
    }
    Ok(emit_enum(spec, &rows))
}

/// Writes the Rust source of one enum from validated `(rust name, C name,
/// value)` rows (non-empty, unique names and values).
fn emit_enum(spec: &EnumSpec, rows: &[(String, &str, i64)]) -> String {
    let (repr, from_fn) = match spec.repr {
        Repr::U8 => ("u8", "from_u8"),
        Repr::I32 => ("i32", "from_i32"),
    };
    let name = spec.rust_name;
    let mut s = String::new();
    // Writing to a String cannot fail; the results are ignored on purpose.
    let _ = writeln!(s, "/// {}", spec.doc);
    let _ = writeln!(s, "///");
    let _ = writeln!(
        s,
        "/// Generated by `build.rs` from the pinned ka9q-radio headers; the"
    );
    let _ = writeln!(
        s,
        "/// discriminant is the wire value and [`{name}::c_name`] gives the C name."
    );
    let _ = writeln!(
        s,
        "#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]"
    );
    let _ = writeln!(s, "#[repr({repr})]");
    let _ = writeln!(s, "pub enum {name} {{");
    for (rust, c_name, value) in rows {
        let _ = writeln!(s, "    /// `{}` = {value}", doc_escape(c_name));
        let _ = writeln!(s, "    {rust} = {value},");
    }
    let _ = writeln!(s, "}}\n");
    let _ = writeln!(
        s,
        "#[allow(clippy::too_many_lines, reason = \"one match arm per C enumerator\")]"
    );
    let _ = writeln!(s, "impl {name} {{");
    let _ = writeln!(s, "    /// Every variant, in C declaration order.");
    let _ = writeln!(s, "    pub const ALL: &'static [Self] = &[");
    for (rust, _, _) in rows {
        let _ = writeln!(s, "        Self::{rust},");
    }
    let _ = writeln!(s, "    ];\n");
    let _ = writeln!(s, "    /// The wire value.");
    let _ = writeln!(s, "    pub const fn value(self) -> {repr} {{");
    let _ = writeln!(s, "        self as {repr}");
    let _ = writeln!(s, "    }}\n");
    let _ = writeln!(
        s,
        "    /// Maps a wire value to a variant; `None` for unknown values."
    );
    let _ = writeln!(
        s,
        "    pub const fn {from_fn}(v: {repr}) -> Option<Self> {{"
    );
    let _ = writeln!(s, "        match v {{");
    for (rust, _, value) in rows {
        let _ = writeln!(s, "            {value} => Some(Self::{rust}),");
    }
    let _ = writeln!(s, "            _ => None,");
    let _ = writeln!(s, "        }}");
    let _ = writeln!(s, "    }}\n");
    let _ = writeln!(
        s,
        "    /// The C enumerator name, e.g. `\"{}\"`.",
        rows[0].1
    );
    let _ = writeln!(s, "    pub const fn c_name(self) -> &'static str {{");
    let _ = writeln!(s, "        match self {{");
    for (rust, c_name, _) in rows {
        let _ = writeln!(s, "            Self::{rust} => \"{}\",", doc_escape(c_name));
    }
    let _ = writeln!(s, "        }}");
    let _ = writeln!(s, "    }}");
    let _ = writeln!(s, "}}\n");
    let _ = writeln!(s, "impl ::core::fmt::Display for {name} {{");
    let _ = writeln!(s, "    /// Formats as the C enumerator name.");
    let _ = writeln!(
        s,
        "    fn fmt(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {{"
    );
    let _ = writeln!(s, "        f.write_str(self.c_name())");
    let _ = writeln!(s, "    }}");
    let _ = writeln!(s, "}}\n");
    s
}

/// The parsed contents of `vendor/ka9q-radio/VERSION`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionFile {
    /// Upstream repository URL.
    pub upstream: String,
    /// Pinned upstream commit (40 hex digits).
    pub commit: String,
    /// `(file name, lowercase hex SHA-256)` of each vendored file.
    pub sha256: Vec<(String, String)>,
}

/// Parses the `VERSION` file written by `scripts/update-ka9q-headers.sh`:
/// `#` comments, `upstream <url>`, `commit <sha1>`, `sha256 <hex> <file>`.
pub fn parse_version_file(text: &str) -> Result<VersionFile, String> {
    let mut upstream = None;
    let mut commit = None;
    let mut sha256 = Vec::new();
    for line in text.lines().map(str::trim) {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let words: Vec<&str> = line.split_whitespace().collect();
        match words.as_slice() {
            ["upstream", url] => upstream = Some((*url).to_owned()),
            ["commit", c] if c.len() == 40 && c.bytes().all(|b| b.is_ascii_hexdigit()) => {
                commit = Some((*c).to_owned());
            }
            ["sha256", hex, file]
                if hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit()) =>
            {
                sha256.push(((*file).to_owned(), hex.to_ascii_lowercase()));
            }
            _ => return Err(format!("VERSION: unrecognized line `{line}`")),
        }
    }
    Ok(VersionFile {
        upstream: upstream.ok_or("VERSION: missing `upstream` line")?,
        commit: commit.ok_or("VERSION: missing `commit` line")?,
        sha256,
    })
}

/// Generates the whole `tags.rs` include from the vendored header sources.
/// `headers` maps a file name (`status.h`, `rtp.h`) to its contents.
pub fn generate_all(headers: &[(&str, &str)], version: &VersionFile) -> Result<String, String> {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "// @generated by backend-ka9q build.rs from vendor/ka9q-radio. Do not edit.\n"
    );
    let _ = writeln!(
        out,
        "/// Upstream ka9q-radio repository of the pinned headers."
    );
    let _ = writeln!(
        out,
        "pub const UPSTREAM_URL: &str = \"{}\";\n",
        doc_escape(&version.upstream)
    );
    let _ = writeln!(
        out,
        "/// ka9q-radio commit the tag enums were generated from."
    );
    let _ = writeln!(
        out,
        "pub const UPSTREAM_COMMIT: &str = \"{}\";\n",
        version.commit
    );
    for (file, spec) in SPECS {
        let Some((_, src)) = headers.iter().find(|(f, _)| f == file) else {
            return Err(format!("header {file} was not provided"));
        };
        let enums = parse_enums(src).map_err(|e| format!("{file}: {e}"))?;
        let Some(c) = enums.iter().find(|e| e.name == spec.c_name) else {
            return Err(format!("{file}: expected `enum {}` not found", spec.c_name));
        };
        out.push_str(&generate_enum(spec, c).map_err(|e| format!("{file}: {e}"))?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(e: &CEnum) -> Vec<(&str, i64)> {
        e.variants
            .iter()
            .map(|v| (v.name.as_str(), v.value))
            .collect()
    }

    #[test]
    fn strips_line_and_block_comments() {
        let src = "a // x\nb /* y\n z */ c /**/d";
        assert_eq!(strip_comments(src).unwrap(), "a \nb \n  c  d");
    }

    #[test]
    fn keeps_comment_markers_inside_literals() {
        let src = r#"x "a // b /* c" 'q' "esc \" //" y // gone"#;
        assert_eq!(
            strip_comments(src).unwrap(),
            r#"x "a // b /* c" 'q' "esc \" //" y "#
        );
    }

    #[test]
    fn rejects_unterminated_comment_and_literal() {
        assert!(
            strip_comments("a /* b")
                .unwrap_err()
                .contains("unterminated")
        );
        assert!(
            strip_comments("a \"b")
                .unwrap_err()
                .contains("unterminated")
        );
    }

    #[test]
    fn parses_implicit_explicit_negative_values_and_comments() {
        let src = "
            // leading
            enum demo {
              A = -1, // sentinel
              B = 0,
              /* block */ C,

              D = 0x10,
              E /* inline */ ,
              F = 010,
              G = 7u,
            };
            enum demo x; // a use, not a definition
        ";
        let enums = parse_enums(src).unwrap();
        assert_eq!(enums.len(), 1);
        assert_eq!(enums[0].name, "demo");
        assert_eq!(
            names(&enums[0]),
            vec![
                ("A", -1),
                ("B", 0),
                ("C", 1),
                ("D", 16),
                ("E", 17),
                ("F", 8),
                ("G", 7)
            ]
        );
    }

    #[test]
    fn handles_no_trailing_comma_and_multiple_enums() {
        let src = "enum a { X, Y } ; enum b{Z=5};";
        let enums = parse_enums(src).unwrap();
        assert_eq!(enums.len(), 2);
        assert_eq!(names(&enums[0]), vec![("X", 0), ("Y", 1)]);
        assert_eq!(names(&enums[1]), vec![("Z", 5)]);
    }

    #[test]
    fn ignores_identifiers_containing_enum() {
        let src = "int myenum { }; enumeration e; enum ok { A };";
        let enums = parse_enums(src).unwrap();
        assert_eq!(enums.len(), 1);
        assert_eq!(enums[0].name, "ok");
    }

    #[test]
    fn rejects_unsupported_bodies() {
        assert!(
            parse_enums("enum a { X = Y };")
                .unwrap_err()
                .contains("unsupported enumerator value")
        );
        assert!(
            parse_enums("enum a { X Y };")
                .unwrap_err()
                .contains("unsupported enumerator")
        );
        assert!(
            parse_enums("enum a { X, X };")
                .unwrap_err()
                .contains("duplicate")
        );
        assert!(
            parse_enums("enum a { X ")
                .unwrap_err()
                .contains("closing brace")
        );
        assert!(
            parse_enums("enum a { X = 9223372036854775807, Y };")
                .unwrap_err()
                .contains("overflow")
        );
    }

    #[test]
    fn parse_int_forms() {
        assert_eq!(parse_int("0").unwrap(), 0);
        assert_eq!(parse_int(" -1 ").unwrap(), -1);
        assert_eq!(parse_int("+3").unwrap(), 3);
        assert_eq!(parse_int("0XfF").unwrap(), 255);
        assert_eq!(parse_int("017").unwrap(), 15);
        assert_eq!(parse_int("42UL").unwrap(), 42);
        assert!(parse_int("1+1").is_err());
        assert!(parse_int("0x-5").is_err());
        assert!(parse_int("--1").is_err());
        assert!(parse_int("").is_err());
    }

    #[test]
    fn camel_case() {
        assert_eq!(to_camel_case("RADIO_FREQUENCY"), "RadioFrequency");
        assert_eq!(to_camel_case("OPUS_BIT_RATE"), "OpusBitRate");
        assert_eq!(to_camel_case("EOL"), "Eol");
        assert_eq!(to_camel_case("S16LE"), "S16le");
        assert_eq!(to_camel_case("UNUSED4"), "Unused4");
        assert_eq!(to_camel_case("_A__B_"), "AB");
    }

    const SPEC_U8: EnumSpec = EnumSpec {
        c_name: "t",
        rust_name: "T",
        repr: Repr::U8,
        skip: &[],
        strip_suffix: None,
        doc: "Test.",
    };

    fn one(src: &str) -> CEnum {
        parse_enums(src).unwrap().remove(0)
    }

    #[test]
    fn generate_u8_enum() {
        let out = generate_enum(&SPEC_U8, &one("enum t { EOL = 0, ONE_TWO, };")).unwrap();
        assert!(out.contains("#[repr(u8)]"));
        assert!(out.contains("    Eol = 0,"));
        assert!(out.contains("    OneTwo = 1,"));
        assert!(out.contains("pub const fn from_u8(v: u8) -> Option<Self>"));
        assert!(out.contains("Self::OneTwo => \"ONE_TWO\","));
        assert!(out.contains("impl ::core::fmt::Display for T"));
    }

    #[test]
    fn generate_i32_enum_with_skip_and_suffix() {
        let spec = EnumSpec {
            repr: Repr::I32,
            skip: &["BAD_X", "N_X"],
            strip_suffix: Some("_X"),
            ..SPEC_U8
        };
        let out = generate_enum(&spec, &one("enum t { BAD_X = -1, A_X = 0, B2_X, N_X };")).unwrap();
        assert!(out.contains("#[repr(i32)]"));
        assert!(out.contains("    A = 0,"));
        assert!(out.contains("    B2 = 1,"));
        assert!(out.contains("pub const fn from_i32(v: i32) -> Option<Self>"));
        assert!(!out.contains("BAD_X"));
        assert!(!out.contains("N_X"));
    }

    #[test]
    fn generate_rejects_bad_enums() {
        let too_big = generate_enum(&SPEC_U8, &one("enum t { A = 255, B };")).unwrap_err();
        assert!(too_big.contains("does not fit in a u8"), "{too_big}");
        let negative = generate_enum(&SPEC_U8, &one("enum t { A = -1 };")).unwrap_err();
        assert!(negative.contains("does not fit in a u8"));
        let i32_spec = EnumSpec {
            repr: Repr::I32,
            ..SPEC_U8
        };
        assert!(
            generate_enum(&i32_spec, &one("enum t { A = 4294967296 };"))
                .unwrap_err()
                .contains("i32")
        );
        assert!(
            generate_enum(&SPEC_U8, &one("enum t { A = 1, B = 1 };"))
                .unwrap_err()
                .contains("share the value")
        );
        assert!(
            generate_enum(&SPEC_U8, &one("enum t { A_B, A__B };"))
                .unwrap_err()
                .contains("both map")
        );
        assert!(
            generate_enum(&SPEC_U8, &one("enum t { _1 };"))
                .unwrap_err()
                .contains("cannot name")
        );
        assert!(
            generate_enum(&SPEC_U8, &one("enum t { };"))
                .unwrap_err()
                .contains("no variants")
        );
        let skip = EnumSpec {
            skip: &["GONE"],
            ..SPEC_U8
        };
        assert!(
            generate_enum(&skip, &one("enum t { A };"))
                .unwrap_err()
                .contains("GONE is missing")
        );
    }

    const VERSION: &str = "# comment\n\nupstream https://example.org/r\ncommit 384b269aaf5e9ee1015185ae7c0cdd60f4d2ecbd\nsha256 98ef42d26c291323eb41ff90dedda69bc13c900370b0fd0af2098ff2305935a8 status.h\n";

    #[test]
    fn version_file() {
        let v = parse_version_file(VERSION).unwrap();
        assert_eq!(v.upstream, "https://example.org/r");
        assert_eq!(v.commit, "384b269aaf5e9ee1015185ae7c0cdd60f4d2ecbd");
        assert_eq!(v.sha256.len(), 1);
        assert_eq!(v.sha256[0].0, "status.h");
        assert!(
            parse_version_file("commit 384b269aaf5e9ee1015185ae7c0cdd60f4d2ecbd")
                .unwrap_err()
                .contains("upstream")
        );
        assert!(
            parse_version_file("upstream u")
                .unwrap_err()
                .contains("commit")
        );
        assert!(
            parse_version_file("commit short")
                .unwrap_err()
                .contains("unrecognized")
        );
    }

    #[test]
    fn generate_all_reports_missing_pieces() {
        let v = parse_version_file(VERSION).unwrap();
        assert!(
            generate_all(&[("status.h", "")], &v)
                .unwrap_err()
                .contains("expected `enum pkt_type` not found")
        );
        assert!(generate_all(&[], &v).unwrap_err().contains("not provided"));
        assert!(
            generate_all(&[("status.h", "/*")], &v)
                .unwrap_err()
                .contains("status.h: unterminated")
        );
    }

    #[test]
    fn generate_all_from_vendored_headers() {
        let status = include_str!("../vendor/ka9q-radio/status.h");
        let rtp = include_str!("../vendor/ka9q-radio/rtp.h");
        let v = parse_version_file(include_str!("../vendor/ka9q-radio/VERSION")).unwrap();
        let out = generate_all(&[("status.h", status), ("rtp.h", rtp)], &v).unwrap();
        for ty in ["PktType", "StatusType", "DemodType", "Encoding"] {
            assert!(out.contains(&format!("pub enum {ty} {{")), "{ty}");
        }
        assert!(out.contains("RadioFrequency = 33,"));
        assert!(out.contains("Spect2 = 4,"));
        assert!(out.contains("Opus = 3,"));
        assert!(!out.contains("UnusedEncoding"));
    }
}
