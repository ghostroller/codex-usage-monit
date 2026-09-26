//! Bounded plist reading for launchd identity checks. Generation and the raw
//! byte fingerprint deliberately remain in the parent module.

use std::collections::HashMap;

use anyhow::{Context, Result, bail};
use quick_xml::Reader;
use quick_xml::events::{BytesStart, Event};

const MAX_DEPTH: usize = 32;

pub(super) struct Definition {
    pub arguments: Vec<String>,
    pub environment_path: Option<String>,
}

enum Value {
    String(String),
    Array(Vec<Value>),
    Dictionary(HashMap<String, Value>),
    Other,
}

pub(super) fn parse(contents: &[u8]) -> Result<Definition> {
    if contents.len() as u64 > super::SERVICE_DEFINITION_MAX_BYTES {
        bail!("launchd definition exceeds the size limit");
    }
    let document = std::str::from_utf8(contents).context("launchd definition is not UTF-8")?;
    if !document.chars().all(valid_xml_character) {
        bail!("launchd definition contains an invalid XML character");
    }
    let mut reader = Reader::from_str(document);
    reader.config_mut().expand_empty_elements = true;
    reader.config_mut().check_comments = true;
    let mut declaration_allowed = true;
    let mut doctype_seen = false;
    let root = loop {
        match reader.read_event().context("could not parse launchd XML")? {
            Event::Decl(declaration) if declaration_allowed => {
                declaration_allowed = false;
                if declaration.version()?.as_ref() != b"1.0" {
                    bail!("launchd definition requires XML 1.0");
                }
                let content = std::str::from_utf8(declaration.as_ref())?;
                let attributes = BytesStart::from_content(content, 3);
                let mut previous = 0;
                for attribute in attributes.attributes() {
                    let attribute = attribute.context("invalid launchd XML declaration")?;
                    let order = match attribute.key.as_ref() {
                        b"version" if attribute.value.as_ref() == b"1.0" => 1,
                        b"encoding" if attribute.value.eq_ignore_ascii_case(b"UTF-8") => 2,
                        b"standalone" if matches!(attribute.value.as_ref(), b"yes" | b"no") => 3,
                        _ => bail!("unsupported launchd XML declaration"),
                    };
                    if order <= previous {
                        bail!("invalid launchd XML declaration attribute order");
                    }
                    previous = order;
                }
            }
            Event::DocType(doctype) if !doctype_seen => {
                declaration_allowed = false;
                doctype_seen = true;
                validate_doctype(std::str::from_utf8(doctype.as_ref())?)?;
            }
            Event::Comment(_) => declaration_allowed = false,
            Event::Text(text) if text.as_ref().iter().all(u8::is_ascii_whitespace) => {
                declaration_allowed = false;
            }
            Event::Start(element) if element.name().as_ref() == b"plist" => {
                let mut version_seen = false;
                for attribute in element.attributes() {
                    let attribute = attribute.context("invalid launchd plist attribute")?;
                    if attribute.key.as_ref() != b"version" || attribute.value.as_ref() != b"1.0" {
                        bail!("unsupported launchd plist attribute");
                    }
                    version_seen = true;
                }
                if !version_seen {
                    bail!("launchd plist lacks version 1.0");
                }
                break element;
            }
            _ => bail!("launchd definition must have one plist root"),
        }
    };
    let Event::Start(dictionary) = next_content(&mut reader)? else {
        bail!("launchd plist must contain a dictionary");
    };
    let Value::Dictionary(mut values) = read_value(&mut reader, dictionary, 2)? else {
        bail!("launchd plist must contain a dictionary");
    };
    match next_content(&mut reader)? {
        Event::End(end) if end.name() == root.name() => {}
        _ => bail!("launchd plist must contain exactly one dictionary"),
    }
    if !matches!(next_content(&mut reader)?, Event::Eof) {
        bail!("launchd definition contains content after its plist root");
    }
    let Some(Value::Array(arguments)) = values.remove("ProgramArguments") else {
        bail!("launchd ProgramArguments must be an array");
    };
    let arguments = arguments
        .into_iter()
        .map(|value| match value {
            Value::String(argument) => Ok(argument),
            _ => bail!("launchd ProgramArguments must contain only strings"),
        })
        .collect::<Result<_>>()?;
    let environment_path = match values.remove("EnvironmentVariables") {
        None => None,
        Some(Value::Dictionary(mut environment)) => match environment.remove("PATH") {
            None => None,
            Some(Value::String(path)) => Some(path),
            Some(_) => bail!("launchd EnvironmentVariables PATH must be a string"),
        },
        Some(_) => bail!("launchd EnvironmentVariables must be a dictionary"),
    };
    Ok(Definition {
        arguments,
        environment_path,
    })
}

// Only the standard public identifier is accepted. The URL is recognized as
// inert metadata: this reader never loads a DTD or resolves external entities.
fn validate_doctype(doctype: &str) -> Result<()> {
    let mut rest = doctype.trim();
    for token in ["plist", "PUBLIC"] {
        rest = rest
            .strip_prefix(token)
            .filter(|rest| rest.starts_with(char::is_whitespace))
            .context("unsupported launchd plist DOCTYPE")?
            .trim_start();
    }
    let mut quoted = || -> Result<&str> {
        let quote = rest.chars().next().context("truncated plist DOCTYPE")?;
        if !matches!(quote, '\'' | '"') {
            bail!("invalid launchd plist DOCTYPE identifier");
        }
        let (value, remainder) = rest[1..]
            .split_once(quote)
            .context("unterminated plist DOCTYPE identifier")?;
        if !remainder.is_empty() && !remainder.starts_with(char::is_whitespace) {
            bail!("launchd plist DOCTYPE identifiers require whitespace");
        }
        rest = remainder.trim_start();
        Ok(value)
    };
    if quoted()? != "-//Apple//DTD PLIST 1.0//EN"
        || !matches!(
            quoted()?,
            "http://www.apple.com/DTDs/PropertyList-1.0.dtd"
                | "https://www.apple.com/DTDs/PropertyList-1.0.dtd"
        )
        || !rest.is_empty()
    {
        bail!("unsupported launchd plist DOCTYPE or internal subset");
    }
    Ok(())
}

fn next_content<'a>(reader: &mut Reader<&'a [u8]>) -> Result<Event<'a>> {
    loop {
        let event = reader.read_event().context("could not parse launchd XML")?;
        match event {
            Event::Comment(_) => continue,
            Event::Text(ref text) if text.as_ref().iter().all(u8::is_ascii_whitespace) => continue,
            _ => return Ok(event),
        }
    }
}

fn read_value(reader: &mut Reader<&[u8]>, element: BytesStart<'_>, depth: usize) -> Result<Value> {
    if depth > MAX_DEPTH {
        bail!("launchd plist exceeds the nesting depth limit");
    }
    if element.attributes().next().is_some() {
        bail!("launchd plist values must not have attributes");
    }
    match element.name().as_ref() {
        b"dict" => {
            let mut values = HashMap::new();
            loop {
                let key = match next_content(reader)? {
                    Event::End(end) if end.name() == element.name() => break,
                    Event::Start(key) if key.name().as_ref() == b"key" => {
                        if depth + 1 > MAX_DEPTH || key.attributes().next().is_some() {
                            bail!("invalid launchd dictionary key");
                        }
                        read_text(reader, &key)?
                    }
                    _ => bail!("launchd dictionary requires a key before every value"),
                };
                let Event::Start(value) = next_content(reader)? else {
                    bail!("launchd dictionary key lacks a value");
                };
                let value = read_value(reader, value, depth + 1)?;
                if values.insert(key, value).is_some() {
                    bail!("launchd dictionary contains a duplicate key");
                }
            }
            Ok(Value::Dictionary(values))
        }
        b"array" => {
            let mut values = Vec::new();
            loop {
                match next_content(reader)? {
                    Event::End(end) if end.name() == element.name() => break,
                    Event::Start(value) => values.push(read_value(reader, value, depth + 1)?),
                    _ => bail!("launchd array contains invalid content"),
                }
            }
            Ok(Value::Array(values))
        }
        b"string" => Ok(Value::String(read_text(reader, &element)?)),
        b"true" | b"false" => {
            if !read_text(reader, &element)?.trim().is_empty() {
                bail!("launchd plist boolean contains text");
            }
            Ok(Value::Other)
        }
        // These plist scalar fields do not contribute to executable identity.
        // Their XML structure is checked, but their application semantics remain
        // launchd's responsibility, just as for unconsumed dictionary keys.
        b"integer" | b"real" | b"date" | b"data" => {
            read_text(reader, &element)?;
            Ok(Value::Other)
        }
        _ => bail!("unsupported launchd plist value element"),
    }
}

fn read_text(reader: &mut Reader<&[u8]>, element: &BytesStart<'_>) -> Result<String> {
    let mut value = String::new();
    loop {
        match reader
            .read_event()
            .context("could not parse launchd XML text")?
        {
            Event::Text(text) => {
                if text.as_ref().windows(3).any(|bytes| bytes == b"]]>") {
                    bail!("launchd XML text contains a literal CDATA terminator");
                }
                value.push_str(&text.xml10_content()?);
            }
            Event::CData(text) => value.push_str(&text.xml10_content()?),
            Event::GeneralRef(reference) => {
                if let Some(character) = reference.resolve_char_ref()? {
                    if !valid_xml_character(character) {
                        bail!("invalid launchd XML character reference");
                    }
                    value.push(character);
                } else {
                    value.push(match reference.as_ref() {
                        b"amp" => '&',
                        b"lt" => '<',
                        b"gt" => '>',
                        b"quot" => '"',
                        b"apos" => '\'',
                        _ => bail!("unsupported launchd XML entity"),
                    });
                }
            }
            Event::Comment(_) => {}
            Event::End(end) if end.name() == element.name() => return Ok(value),
            _ => bail!("launchd plist scalar contains invalid or unterminated content"),
        }
    }
}

fn valid_xml_character(character: char) -> bool {
    matches!(character, '\t' | '\n' | '\r' | '\u{20}'..='\u{d7ff}' | '\u{e000}'..='\u{fffd}' | '\u{10000}'..='\u{10ffff}')
}

#[cfg(test)]
mod tests {
    use super::*;

    const ARGUMENTS: &str =
        "<key>ProgramArguments</key><array><string>/bin/monitor</string></array>";

    fn plist(body: &str) -> String {
        format!("<plist version='1.0'><dict>{body}</dict></plist>")
    }

    #[test]
    fn launchd_plist_accepts_generated_and_standard_apple_doctype() {
        let golden = include_str!("fixtures/launchd-golden.plist");
        for document in [
            golden.to_string(),
            golden.replace("https://www.apple.com", "http://www.apple.com"),
            golden.replace('"', "'"),
            golden.replace("<?xml version=\"1.0\" encoding=\"UTF-8\"?>", ""),
        ] {
            let definition = parse(document.as_bytes()).unwrap();
            assert_eq!(definition.arguments.len(), 19);
            assert_eq!(definition.arguments[0], "/opt/Codex & tools/monitor");
            assert_eq!(
                definition.environment_path.as_deref(),
                Some("/opt/a & b:/usr/bin")
            );
        }
    }

    #[test]
    fn launchd_plist_decodes_entities_cdata_comments_and_xml_line_endings() {
        let document = plist(concat!(
            "<key>Program&#65;rguments</key><array>",
            "<string> &amp;&lt;&gt;&quot;&apos;&#65;&#x1F600; </string>",
            "<string>a\r\nb\rc\nd&#13;e</string>",
            "<string><![CDATA[x<&]]><!-- comment -->y</string>",
            "<string/></array>",
            "<key>EnvironmentVariables</key><dict><key>PATH</key><string/></dict>"
        ));
        let definition = parse(document.as_bytes()).unwrap();
        assert_eq!(
            definition.arguments,
            [" &<>\"'A😀 ", "a\nb\nc\nd\re", "x<&y", ""]
        );
        assert_eq!(definition.environment_path.as_deref(), Some(""));
    }

    #[test]
    fn launchd_plist_respects_dictionary_scope_and_optional_environment() {
        for suffix in [
            "",
            "<key>EnvironmentVariables</key><dict/>",
            "<key>PATH</key><string>not-the-environment</string>",
            "<key>Other</key><dict><key>PATH</key><string>nested</string><key>ProgramArguments</key><array/></dict>",
        ] {
            let definition = parse(plist(&format!("{ARGUMENTS}{suffix}")).as_bytes()).unwrap();
            assert_eq!(definition.arguments, ["/bin/monitor"]);
            assert_eq!(definition.environment_path, None);
        }
        assert!(
            parse(
                plist("<key>Other</key><dict><key>ProgramArguments</key><array/></dict>")
                    .as_bytes()
            )
            .is_err()
        );
        let empty = parse(plist("<key>ProgramArguments</key><array/>").as_bytes()).unwrap();
        assert!(empty.arguments.is_empty());
    }

    #[test]
    fn launchd_plist_rejects_duplicate_keys_at_each_dictionary_depth() {
        for suffix in [
            "<key>ProgramArguments</key><array/>",
            "<key>Program&#65;rguments</key><array/>",
            "<key>Other</key><true/><key>Other</key><false/>",
            "<key>EnvironmentVariables</key><dict/><key>EnvironmentVariables</key><dict/>",
            "<key>EnvironmentVariables</key><dict><key>PATH</key><string>a</string><key>PATH</key><string>b</string></dict>",
            "<key>Other</key><dict><key>x</key><true/><key>x</key><false/></dict>",
        ] {
            assert!(
                parse(plist(&format!("{ARGUMENTS}{suffix}")).as_bytes()).is_err(),
                "{suffix}"
            );
        }
    }

    #[test]
    fn launchd_plist_rejects_wrong_types_pairing_and_nesting() {
        for body in [
            "<key>ProgramArguments</key><string>bad</string>",
            "<key>ProgramArguments</key><array><integer>1</integer></array>",
            "<key>ProgramArguments</key><array><dict/></array>",
            "<key>ProgramArguments</key><array><key>bad</key></array>",
            "<key>ProgramArguments</key><array><string><string>nested</string></string></array>",
            "<key>ProgramArguments</key><key>unpaired</key><array/>",
            "<key>ProgramArguments</key>",
            "<string>unkeyed</string>",
            "<key><string>nested</string></key><array/>",
        ] {
            assert!(parse(plist(body).as_bytes()).is_err(), "{body}");
        }
        for suffix in [
            "<key>EnvironmentVariables</key><string>bad</string>",
            "<key>EnvironmentVariables</key><dict><key>PATH</key><array><string>x</string></array></dict>",
            "<key>EnvironmentVariables</key><dict><key>PATH</key></dict>",
            "<key>Other</key><true>text</true>",
            "<key>Other</key><wrapper><string>x</string></wrapper>",
        ] {
            assert!(
                parse(plist(&format!("{ARGUMENTS}{suffix}")).as_bytes()).is_err(),
                "{suffix}"
            );
        }
    }

    #[test]
    fn launchd_plist_rejects_malformed_documents_without_partial_success() {
        let valid = plist(ARGUMENTS);
        for document in [
            String::new(),
            valid.replace("</array>", "</dict>"),
            valid.replace("</plist>", ""),
            valid.replace("<array>", "<array ignored='yes'>"),
            valid.replace("<key>", "<key ignored='yes'>"),
            valid.replace("version='1.0'", "version='1.0' version='1.0'"),
            valid.replace("version='1.0'", "version='1.0' xmlns='other'"),
            valid.replace("<dict>", "<array>"),
            format!("{valid}{valid}"),
            format!("junk{valid}"),
            format!("{valid}junk"),
            valid.replace("</dict>", "<key>truncated</dict>"),
            valid.replace("<array>", "<array>unexpected text"),
            valid.replace("/bin/monitor", "<unfinished"),
            valid.replace("/bin/monitor", "<!-- invalid -- comment -->"),
            valid.replace("/bin/monitor", "<?unsupported instruction?>"),
            valid.replace("/bin/monitor", "&#0;"),
            valid.replace("/bin/monitor", "&#xFFFE;"),
            valid.replace("/bin/monitor", "\0"),
            valid.replace("/bin/monitor", "&undefined;"),
            valid.replace("/bin/monitor", "&unterminated"),
            valid.replace("/bin/monitor", "]]>"),
        ] {
            assert!(parse(document.as_bytes()).is_err(), "{document}");
        }
        assert!(parse(&[0xff, 0xfe]).is_err());
    }

    #[test]
    fn launchd_plist_accepts_only_supported_declarations_and_inert_doctype() {
        let valid = plist(ARGUMENTS);
        for declaration in [
            "<?xml version='1.1'?>",
            "<?xml version='1.0' encoding='UTF-16'?>",
            "<?xml version='1.0' version='1.0'?>",
            "<?xml version='1.0' unknown='yes'?>",
            "<?xml version='1.0' standalone='maybe'?>",
            "<?xml version='1.0' standalone='yes' encoding='UTF-8'?>",
            "<?xml version='1.0'?><?xml version='1.0'?>",
            " <?xml version='1.0'?>",
            "<!-- comment --><?xml version='1.0'?>",
            "<!DOCTYPE plist SYSTEM 'file:///private/secret'>",
            "<!DOCTYPE plist [<!ENTITY external SYSTEM 'https://example.invalid/secret'>]>",
            "<!DOCTYPE plist PUBLIC '-//Apple//DTD PLIST 1.0//EN' 'https://www.apple.com/DTDs/PropertyList-1.0.dtd' [<!ENTITY x 'value'>]>",
            "<!DOCTYPE plist PUBLIC '-//Apple//DTD PLIST 1.0//EN''https://www.apple.com/DTDs/PropertyList-1.0.dtd'>",
        ] {
            assert!(
                parse(format!("{declaration}{valid}").as_bytes()).is_err(),
                "{declaration}"
            );
        }
        for declaration in [
            "<?xml version='1.0'?>",
            "<?xml version='1.0' encoding='utf-8' standalone='yes'?>",
        ] {
            assert!(parse(format!("{declaration}{valid}").as_bytes()).is_ok());
            assert!(
                parse(
                    valid
                        .replace("<dict>", &format!("<dict>{declaration}"))
                        .as_bytes()
                )
                .is_err()
            );
            assert!(parse(format!("{valid}{declaration}").as_bytes()).is_err());
        }
    }

    #[test]
    fn launchd_plist_enforces_exact_size_and_depth_limits() {
        let mut document = plist(ARGUMENTS);
        document.extend(std::iter::repeat_n(
            ' ',
            super::super::SERVICE_DEFINITION_MAX_BYTES as usize - document.len(),
        ));
        assert!(parse(document.as_bytes()).is_ok());
        document.push(' ');
        assert!(parse(document.as_bytes()).is_err());
        for (arrays, accepted) in [(MAX_DEPTH - 3, true), (MAX_DEPTH - 2, false)] {
            let body = format!(
                "{ARGUMENTS}<key>Nested</key>{}<string/>{}",
                "<array>".repeat(arrays),
                "</array>".repeat(arrays)
            );
            assert_eq!(parse(plist(&body).as_bytes()).is_ok(), accepted);
        }
    }

    #[test]
    fn launchd_plist_rejects_every_truncated_golden_prefix() {
        let golden = include_str!("fixtures/launchd-golden.plist").trim_end();
        for end in 0..golden.len() {
            assert!(
                parse(&golden.as_bytes()[..end]).is_err(),
                "prefix length {end}"
            );
        }
    }
}
