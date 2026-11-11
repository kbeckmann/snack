/// Outcome of scanning the buffer for a complete top-level stanza.
enum StanzaEnd
{
    /// A complete stanza ends `end` bytes into the scanned slice.
    Frame(usize),
    Header(usize, Vec<u8>),
    /// A `</stream:stream>` close tag ends `end` bytes into the slice.
    StreamClose(usize),
}

const MAX_HEADER_BYTES: usize = 64 * 1024;
const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;
pub(crate) const MAX_ATTRIBUTES: usize = 256;

#[derive(Debug)]
pub enum FramingError
{
    InvalidText,
    TooLarge,
    Malformed,
    RestrictedXml,
    UnsupportedEncoding,
}

impl FramingError
{
    pub fn condition(&self) -> &'static str {
        match self { Self::InvalidText => "not-well-formed", Self::TooLarge => "policy-violation", Self::Malformed => "not-well-formed", Self::RestrictedXml => "restricted-xml", Self::UnsupportedEncoding => "unsupported-encoding" }
    }
}

impl std::fmt::Display for FramingError
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::InvalidText => "invalid UTF-8 or XML character in stream",
            Self::TooLarge => "XML stream header, stanza or nesting exceeds size limit",
            Self::Malformed => "malformed XML framing",
            Self::RestrictedXml => "XML markup is prohibited in XMPP streams",
            Self::UnsupportedEncoding => "XMPP streams require UTF-8 encoding",
        })
    }
}

pub struct XmlFramer
{
    // Raw bytes are buffered (rather than a lossily-decoded `String`) so that
    // multi-byte UTF-8 characters split across TCP reads are not corrupted.
    // Decoding to text only happens once a complete frame has been carved out.
    buffer: Vec<u8>,
    // Offset of the first unconsumed byte. Consumed frames are not physically
    // removed on every extraction (which would be O(n) each time, O(n²) when
    // many stanzas are buffered); the prefix is reclaimed lazily by `compact`.
    start: usize,
    stream_opened: bool,
    // Framed elements are parsed independently, so carry the namespace scope
    // inherited from the stream header onto each extracted root element.
    namespaces: Vec<(String, String)>,
    deferred: std::collections::VecDeque<String>,
    last_unmanaged: bool,
    error: Option<FramingError>,
    scanner: Scanner,
    stream_name: Vec<u8>,
}

impl XmlFramer
{
    pub fn new() -> Self
    {
        return Self { buffer: Vec::new(), start: 0, stream_opened: false, namespaces: Vec::new(), deferred: Default::default(), last_unmanaged: false, error: None, scanner: Scanner::default(), stream_name: b"stream:stream".to_vec() };
    }

    #[cfg(test)]
    pub fn new_opened() -> Self
    {
        return Self { buffer: Vec::new(), start: 0, stream_opened: true, namespaces: Vec::new(), deferred: Default::default(), last_unmanaged: false, error: None, scanner: Scanner::default(), stream_name: b"stream:stream".to_vec() };
    }

    #[cfg(test)]
    pub fn new_client_stream() -> Self
    {
        let mut framer = Self::new();
        framer.feed(b"<stream:stream xmlns='jabber:client' xmlns:stream='http://etherx.jabber.org/streams'>");
        assert!(framer.try_next().is_some());
        framer
    }

    pub fn feed(&mut self, data: &[u8])
    {
        if self.error.is_none() { self.buffer.extend_from_slice(data); }
    }

    pub fn reset(&mut self)
    {
        self.buffer.clear();
        self.start = 0;
        self.stream_opened = false;
        self.namespaces.clear();
        self.deferred.clear();
        self.last_unmanaged = false;
        self.error = None;
        self.scanner = Scanner::default();
        self.stream_name = b"stream:stream".to_vec();
    }

    pub fn error(&self) -> Option<&FramingError> { self.error.as_ref() }

    /// Put setup-time stanzas back ahead of unread socket data, preserving order.
    /// These arrived before stream management was enabled and must not be counted.
    pub fn defer_frames(&mut self, frames: Vec<String>)
    {
        for frame in frames.into_iter().rev() { self.deferred.push_front(frame); }
    }

    pub fn last_frame_unmanaged(&self) -> bool { self.last_unmanaged }
    pub fn stream_is_open(&self) -> bool { self.stream_opened }

    /// The unconsumed portion of the buffer.
    fn active(&self) -> &[u8]
    {
        return &self.buffer[self.start..];
    }

    /// Consume `n` bytes from the front of the active region, returning them
    /// decoded as a string. A complete frame is delimited by ASCII `<`/`>`, so
    /// any multi-byte characters inside it are whole by construction.
    fn advance(&mut self, n: usize) -> Option<String>
    {
        let end = self.start + n;
        let text = std::str::from_utf8(&self.buffer[self.start..end]);
        let Ok(text) = text else { self.error = Some(FramingError::InvalidText); return None; };
        if !valid_xml_text(text) {
            self.error = Some(FramingError::InvalidText);
            return None;
        }
        let frame = text.to_string();
        self.start = end;
        self.compact();
        return Some(frame);
    }

    /// Reclaim the consumed prefix once it dominates the buffer. This keeps the
    /// amortised cost of consuming a frame O(1) while bounding memory use.
    fn compact(&mut self)
    {
        if self.start == self.buffer.len()
        {
            self.buffer.clear();
            self.start = 0;
        }
        else if self.start >= self.buffer.len() / 2
        {
            self.buffer.drain(..self.start);
            self.start = 0;
        }
    }

    /// Try to extract the next frame from the buffer.
    /// Before the stream is opened, this extracts the `<stream:stream ...>` header.
    /// After the stream is opened, this extracts complete top-level stanzas.
    pub fn try_next(&mut self) -> Option<String>
    {
        if self.error.is_some() { return None; }
        self.last_unmanaged = false;
        if let Some(frame) = self.deferred.pop_front() {
            self.last_unmanaged = true;
            return Some(frame);
        }
        if self.stream_opened { self.skip_whitespace(); }
        if self.active().is_empty()
        {
            return None;
        }

        let limit = if self.stream_opened { MAX_FRAME_BYTES } else { MAX_HEADER_BYTES };
        let active = &self.buffer[self.start..];
        let result = self.scanner.scan(&active[..active.len().min(limit)], !self.stream_opened, &self.stream_name);
        let result = match result {
            Err(error) => { self.error = Some(error); return None; }
            Ok(Some(result)) => result,
            Ok(None) => {
                if self.active().len() >= limit { self.error = Some(FramingError::TooLarge); }
                return None;
            }
        };
        self.scanner.reset();
        match result {
            StanzaEnd::Header(end, name) => {
                let header = self.advance(end)?;
                let header = match crate::stanza::xml::normalize_xml(&header) {
                    Ok(header) => header.into_owned(),
                    Err(_) => { self.error = Some(FramingError::Malformed); return None; }
                };
                self.stream_opened = true;
                self.stream_name = name;
                match stream_namespaces(&header) {
                    Ok(namespaces) => self.namespaces = namespaces,
                    Err(error) => { self.error = Some(error); return None; }
                }
                Some(header)
            }
            StanzaEnd::Frame(end) => {
                let frame = self.advance(end)?;
                let frame = inherit_namespaces(frame, &self.namespaces);
                let frame = match crate::stanza::xml::normalize_xml(&frame) {
                    Ok(frame) => frame.into_owned(),
                    Err(_) => { self.error = Some(FramingError::Malformed); return None; }
                };
                if let Err(error) = validate_frame(&frame) { self.error = Some(error); return None; }
                Some(frame)
            }
            StanzaEnd::StreamClose(end) => {
                self.stream_opened = false;
                self.namespaces.clear();
                self.advance(end)
            }
        }
    }

    fn skip_whitespace(&mut self)
    {
        let ws_len = self.active().iter().take_while(|b| matches!(**b, b' ' | b'\t' | b'\r' | b'\n')).count();
        self.start += ws_len;
    }
}

pub(crate) fn valid_xml_text(text: &str) -> bool
{
    text.chars().all(valid_xml_character)
}

pub(crate) fn valid_xml_character(ch: char) -> bool
{
    matches!(ch, '\t' | '\n' | '\r') || (ch >= ' ' && ch != '\u{fffe}' && ch != '\u{ffff}')
}

// XML 1.0 (fifth edition) NameStartChar/NameChar, with the namespace
// QName restriction: at most one colon, separating two nonempty NCNames.
fn valid_qualified_name(name: &[u8]) -> bool
{
    fn start(ch: char) -> bool {
        matches!(ch, '_' | 'A'..='Z' | 'a'..='z' | '\u{c0}'..='\u{d6}' | '\u{d8}'..='\u{f6}'
            | '\u{f8}'..='\u{2ff}' | '\u{370}'..='\u{37d}' | '\u{37f}'..='\u{1fff}'
            | '\u{200c}'..='\u{200d}' | '\u{2070}'..='\u{218f}' | '\u{2c00}'..='\u{2fef}'
            | '\u{3001}'..='\u{d7ff}' | '\u{f900}'..='\u{fdcf}' | '\u{fdf0}'..='\u{fffd}'
            | '\u{10000}'..='\u{effff}')
    }
    fn ncname(name: &str) -> bool {
        let mut chars = name.chars();
        chars.next().is_some_and(start) && chars.all(|ch| start(ch)
            || matches!(ch, '-' | '.' | '0'..='9' | '\u{b7}' | '\u{300}'..='\u{36f}' | '\u{203f}'..='\u{2040}'))
    }
    let Ok(name) = std::str::from_utf8(name) else { return false; };
    let mut parts = name.split(':');
    ncname(parts.next().unwrap_or_default()) && parts.next().is_none_or(ncname) && parts.next().is_none()
}

fn valid_element_name(name: &[u8]) -> bool
{
    valid_qualified_name(name) && !name.starts_with(b"xmlns:")
}

// Namespace constraints apply even when a declaration is never used. XML 1.0
// permits clearing the default namespace, but never undeclaring a named prefix.
fn validate_namespace_binding(name: &[u8], value: &str) -> Result<(), FramingError>
{
    const XML: &str = "http://www.w3.org/XML/1998/namespace";
    const XMLNS: &str = "http://www.w3.org/2000/xmlns/";
    let invalid = if name == b"xmlns" {
        matches!(value, XML | XMLNS)
    } else if let Some(prefix) = name.strip_prefix(b"xmlns:") {
        value.is_empty() || prefix == b"xmlns"
            || if prefix == b"xml" { value != XML } else { matches!(value, XML | XMLNS) }
    } else { false };
    if invalid { Err(FramingError::Malformed) } else { Ok(()) }
}

fn validate_frame(frame: &str) -> Result<(), FramingError>
{
    use quick_xml::{events::Event, NsReader};
    let mut reader = NsReader::from_str(frame);
    loop {
        let event = reader.read_event().map_err(|_| FramingError::Malformed)?;
        validate_node(&reader, &event)?;
        if matches!(event, Event::Eof) { return Ok(()); }
    }
}

// The framed-stream and standalone protocol parsers must enforce identical
// XML rules. Validate during their existing event walks, without another pass.
pub(crate) fn validate_node(reader: &quick_xml::NsReader<&[u8]>, event: &quick_xml::events::Event<'_>) -> Result<(), FramingError>
{
    use quick_xml::{events::Event, name::ResolveResult};
    match event {
        Event::Start(element) | Event::Empty(element) => {
            if matches!(reader.resolver().resolve_element(element.name()).0, ResolveResult::Unknown(_))
                || !valid_element_name(element.name().as_ref()) {
                return Err(FramingError::Malformed);
            }
            let mut raw_names = std::collections::HashSet::new();
            let mut expanded_names = std::collections::HashSet::new();
            for (index, attribute) in element.attributes().with_checks(false).enumerate() {
                if index >= MAX_ATTRIBUTES { return Err(FramingError::TooLarge); }
                let attribute = attribute.map_err(|_| FramingError::Malformed)?;
                if !valid_qualified_name(attribute.key.as_ref()) { return Err(FramingError::Malformed); }
                if !raw_names.insert(attribute.key.as_ref().to_vec()) { return Err(FramingError::Malformed); }
                let value = attribute.unescape_value().map_err(|_| FramingError::Malformed)?;
                if !valid_xml_text(&value) { return Err(FramingError::InvalidText); }
                validate_namespace_binding(attribute.key.as_ref(), &value)?;
                if attribute.key.as_ref() == b"xmlns" || attribute.key.as_ref().starts_with(b"xmlns:") { continue; }
                let (namespace, local) = reader.resolver().resolve_attribute(attribute.key);
                let namespace = match namespace {
                    ResolveResult::Unbound => Vec::new(),
                    ResolveResult::Bound(namespace) => namespace.as_ref().to_vec(),
                    ResolveResult::Unknown(_) => return Err(FramingError::Malformed),
                };
                if !expanded_names.insert((namespace, local.as_ref().to_vec())) { return Err(FramingError::Malformed); }
            }
        }
        Event::Text(text) => {
            if !valid_xml_text(&text.decode().map_err(|_| FramingError::Malformed)?) { return Err(FramingError::InvalidText); }
        }
        Event::CData(text) => {
            if !valid_xml_text(&text.decode().map_err(|_| FramingError::Malformed)?) { return Err(FramingError::InvalidText); }
        }
        Event::GeneralRef(reference) => {
            let reference = format!("&{};", reference.decode().map_err(|_| FramingError::Malformed)?);
            let text = quick_xml::escape::unescape(&reference).map_err(|_| FramingError::Malformed)?;
            if !valid_xml_text(&text) { return Err(FramingError::InvalidText); }
        }
        Event::DocType(_) | Event::Decl(_) | Event::Comment(_) | Event::PI(_) => return Err(FramingError::RestrictedXml),
        _ => {},
    }
    Ok(())
}

pub(crate) fn validate_declaration(declaration: &quick_xml::events::BytesDecl<'_>) -> Result<(), FramingError>
{
    let content = std::str::from_utf8(declaration.as_ref()).map_err(|_| FramingError::Malformed)?;
    let attributes = quick_xml::events::BytesStart::from_content(content, 3);
    let mut version = false;
    let mut encoding = false;
    for (index, attribute) in attributes.attributes().enumerate() {
        let attribute = attribute.map_err(|_| FramingError::Malformed)?;
        match attribute.key.as_ref() {
            b"version" if index == 0 && attribute.value.as_ref() == b"1.0" => version = true,
            b"encoding" if version && !encoding => {
                if !attribute.value.as_ref().eq_ignore_ascii_case(b"UTF-8") { return Err(FramingError::UnsupportedEncoding); }
                encoding = true;
            }
            b"standalone" if version && matches!(attribute.value.as_ref(), b"yes" | b"no") => return Err(FramingError::RestrictedXml),
            _ => return Err(FramingError::Malformed),
        }
    }
    if version { Ok(()) } else { Err(FramingError::Malformed) }
}

fn stream_namespaces(header: &str) -> Result<Vec<(String, String)>, FramingError>
{
    use quick_xml::{events::Event, Reader};
    let mut reader = Reader::from_str(header);
    let mut at_start = true;
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                if !valid_element_name(e.name().as_ref()) { return Err(FramingError::Malformed); }
                let mut namespaces = Vec::new();
                for (index, attr) in e.attributes().enumerate() {
                    if index >= MAX_ATTRIBUTES { return Err(FramingError::TooLarge); }
                    let attr = attr.map_err(|_| FramingError::Malformed)?;
                    let key = attr.key.as_ref();
                    if !valid_qualified_name(key) { return Err(FramingError::Malformed); }
                    let value = attr.unescape_value().map_err(|_| FramingError::Malformed)?;
                    if !valid_xml_text(&value) { return Err(FramingError::InvalidText); }
                    validate_namespace_binding(key, &value)?;
                    // Framed stanzas are parsed as independent documents. Keep
                    // inherited language along with namespace scope so text
                    // translations retain their effective XML language.
                    if key == b"xmlns" || key.starts_with(b"xmlns:") || key == b"xml:lang" {
                        let key = std::str::from_utf8(key).map_err(|_| FramingError::InvalidText)?;
                        namespaces.push((key.to_string(), value.into_owned()));
                    }
                }
                return Ok(namespaces);
            }
            Ok(Event::Decl(declaration)) if at_start => {
                validate_declaration(&declaration)?;
                at_start = false;
            }
            Ok(Event::Text(text)) if text.as_ref().iter().all(u8::is_ascii_whitespace) => at_start = false,
            Ok(Event::Comment(_) | Event::PI(_) | Event::DocType(_)) => return Err(FramingError::RestrictedXml),
            _ => return Err(FramingError::Malformed),
        }
    }
}

fn inherit_namespaces(mut frame: String, namespaces: &[(String, String)]) -> String
{
    use quick_xml::{events::Event, Reader};
    if namespaces.is_empty() { return frame; }
    let mut reader = Reader::from_str(&frame);
    let (position, inherited) = loop {
        match reader.read_event() {
            Ok(event @ (Event::Start(_) | Event::Empty(_))) => {
                let empty = matches!(event, Event::Empty(_));
                let e = match event { Event::Start(e) | Event::Empty(e) => e, _ => unreachable!() };
                let mut declared = Vec::new();
                for (index, attr) in e.attributes().enumerate() {
                    if index >= MAX_ATTRIBUTES { return frame; }
                    let Ok(attr) = attr else { return frame; };
                    declared.push(attr.key.as_ref().to_vec());
                }
                let mut inherited = String::new();
                for (key, value) in namespaces {
                    if !declared.iter().any(|name| name.as_slice() == key.as_bytes()) {
                        inherited.push_str(&format!(" {}='{}'", key, crate::stanza::xml::escape_attribute_value(value)));
                    }
                }
                break (reader.buffer_position() as usize - if empty { 2 } else { 1 }, inherited);
            }
            Ok(Event::Eof) | Err(_) => return frame,
            _ => {},
        }
    };
    frame.insert_str(position, &inherited);
    return frame;
}

// The cursor and lexical state survive incomplete socket reads. Even a long
// quoted attribute, declaration or CDATA section is inspected once, not rescanned
// from its opening delimiter after every read.
#[derive(Default)]
struct Scanner
{
    offset: usize,
    mode: ScanMode,
    stack: Vec<Vec<u8>>,
    text_tail: u16,
    #[cfg(test)]
    steps: usize,
}

#[derive(Default)]
enum ScanMode
{
    #[default]
    Text,
    Prefix(usize),
    Tag { start: usize, closing: bool, quote: u8, previous: u8 },
    CData(u32),
    Processing(u8),
}

fn xml_space(byte: u8) -> bool { matches!(byte, b' ' | b'\t' | b'\r' | b'\n') }

impl Scanner
{
    fn reset(&mut self)
    {
        #[cfg(test)]
        let steps = self.steps;
        *self = Self::default();
        #[cfg(test)]
        { self.steps = steps; }
    }

    fn scan(&mut self, buf: &[u8], header: bool, stream_name: &[u8]) -> Result<Option<StanzaEnd>, FramingError>
    {
        while self.offset < buf.len() {
            #[cfg(test)]
            { self.steps += 1; }
            match &mut self.mode {
                ScanMode::Text => {
                    let byte = buf[self.offset];
                    self.offset += 1;
                    if byte == b'<' {
                        self.mode = ScanMode::Prefix(self.offset - 1);
                        self.text_tail = 0;
                    } else {
                        if self.stack.is_empty() && !xml_space(byte) { return Err(FramingError::Malformed); }
                        if self.text_tail == u16::from_be_bytes(*b"]]") && byte == b'>' { return Err(FramingError::Malformed); }
                        self.text_tail = (self.text_tail << 8) | byte as u16;
                    }
                }
                ScanMode::Prefix(start) => {
                    let start = *start;
                    let suffix = &buf[start..];
                    match suffix[1] {
                        b'/' => {
                            self.offset = start + 2;
                            self.mode = ScanMode::Tag { start, closing: true, quote: 0, previous: 0 };
                        }
                        b'?' => {
                            // An XML declaration is allowed only before the
                            // stream root. Every other PI is restricted XML.
                            if !header || !self.stack.is_empty() { return Err(FramingError::RestrictedXml); }
                            if suffix.len() < 6 {
                                if b"<?xml ".starts_with(suffix) || b"<?xml\t".starts_with(suffix)
                                    || b"<?xml\r".starts_with(suffix) || b"<?xml\n".starts_with(suffix) { return Ok(None); }
                                return Err(FramingError::RestrictedXml);
                            }
                            if !suffix.starts_with(b"<?xml") || !xml_space(suffix[5]) { return Err(FramingError::RestrictedXml); }
                            self.offset = start + 2;
                            self.mode = ScanMode::Processing(0);
                        }
                        b'!' => {
                            if suffix.starts_with(b"<!--") || suffix.starts_with(b"<!DOCTYPE") {
                                return Err(FramingError::RestrictedXml);
                            } else if suffix.starts_with(b"<![CDATA[") {
                                if self.stack.is_empty() { return Err(FramingError::Malformed); }
                                self.offset = start + 9;
                                self.mode = ScanMode::CData(0);
                            } else if b"<!--".starts_with(suffix) || b"<![CDATA[".starts_with(suffix) || b"<!DOCTYPE".starts_with(suffix) {
                                return Ok(None);
                            } else { return Err(FramingError::Malformed); }
                        }
                        b'<' | b'>' | b' ' | b'\t' | b'\r' | b'\n' => return Err(FramingError::Malformed),
                        _ => {
                            self.offset = start + 1;
                            self.mode = ScanMode::Tag { start, closing: false, quote: 0, previous: 0 };
                        }
                    }
                }
                ScanMode::Tag { start, closing, quote, previous } => {
                    let byte = buf[self.offset];
                    self.offset += 1;
                    if *quote != 0 {
                        if byte == b'<' { return Err(FramingError::Malformed); }
                        if byte == *quote { *quote = 0; }
                    } else if byte == b'\'' || byte == b'"' {
                        *quote = byte;
                    } else if byte == b'>' {
                        let start = *start;
                        let closing = *closing;
                        let empty = *previous == b'/';
                        let first = start + if closing { 2 } else { 1 };
                        let end = self.offset - 1 - usize::from(empty);
                        let tag = &buf[first..end];
                        let name_end = tag.iter().position(|byte| xml_space(*byte)).unwrap_or(tag.len());
                        let name = &tag[..name_end];
                        if name.is_empty() { return Err(FramingError::Malformed); }
                        if closing {
                            if empty || tag[name_end..].iter().any(|byte| !xml_space(*byte)) { return Err(FramingError::Malformed); }
                            if self.stack.is_empty() {
                                if !header && name == stream_name { return Ok(Some(StanzaEnd::StreamClose(self.offset))); }
                                return Err(FramingError::Malformed);
                            }
                            if self.stack.pop().as_deref() != Some(name) { return Err(FramingError::Malformed); }
                        } else if header {
                            if empty || name.rsplit(|byte| *byte == b':').next() != Some(b"stream".as_slice()) {
                                return Err(FramingError::Malformed);
                            }
                            return Ok(Some(StanzaEnd::Header(self.offset, name.to_vec())));
                        } else if !empty {
                            if self.stack.is_empty() && name == stream_name { return Err(FramingError::Malformed); }
                            if self.stack.len() >= 128 { return Err(FramingError::TooLarge); }
                            self.stack.push(name.to_vec());
                        }
                        self.mode = ScanMode::Text;
                        self.text_tail = 0;
                        if self.stack.is_empty() { return Ok(Some(StanzaEnd::Frame(self.offset))); }
                        continue;
                    } else if byte == b'<' || *previous == b'/' {
                        return Err(FramingError::Malformed);
                    }
                    *previous = byte;
                }
                ScanMode::CData(tail) => {
                    *tail = ((*tail << 8) | buf[self.offset] as u32) & 0x00ff_ffff;
                    self.offset += 1;
                    let done = match self.mode {
                        ScanMode::CData(tail) => tail == u32::from_be_bytes(*b"\0]]>"),
                        _ => false,
                    };
                    if done { self.mode = ScanMode::Text; }
                }
                ScanMode::Processing(previous) => {
                    let byte = buf[self.offset];
                    self.offset += 1;
                    if *previous == b'?' && byte == b'>' { self.mode = ScanMode::Text; }
                    else { *previous = byte; }
                }
            }
        }
        return Ok(None);
    }
}

#[cfg(test)]
mod tests
{
    use super::*;

    #[test]
    fn framing_is_independent_of_socket_chunk_boundaries()
    {
        let stream = "<?xml version='1.0'?><s:stream xmlns:s='http://etherx.jabber.org/streams' xmlns='jabber:client' note='a>b'>\n<message note='not /> yet'><body>héllo &amp; 😀</body><x xmlns='urn:test'/></message> \n<presence><status><![CDATA[<ready>]]></status></presence></s:stream>";
        let mut whole = XmlFramer::new();
        whole.feed(stream.as_bytes());
        let mut expected = Vec::new();
        while let Some(frame) = whole.try_next() { expected.push(frame); }
        assert!(whole.error().is_none());
        assert_eq!(expected.len(), 4);
        for size in 1..=stream.len() {
            let mut framer = XmlFramer::new();
            let mut frames = Vec::new();
            for chunk in stream.as_bytes().chunks(size) {
                framer.feed(chunk);
                while let Some(frame) = framer.try_next() { frames.push(frame); }
                assert!(framer.error().is_none(), "chunk size {size}");
            }
            assert_eq!(frames, expected, "chunk size {size}");
            assert!(framer.scanner.steps <= stream.len() * 4);
        }
    }

    #[test]
    fn byte_fragmentation_scans_long_attributes_text_and_cdata_linearly()
    {
        for stanza in [
            format!("<message note='{}'><body>done</body></message>", "x".repeat(40_000)),
            format!("<message><body><![CDATA[{}]]></body></message>", "<a>&😀".repeat(5_000)),
            format!("<message><body>{}</body></message>", "x".repeat(40_000)),
        ] {
            let mut framer = XmlFramer::new_opened();
            for (i, byte) in stanza.as_bytes().iter().enumerate() {
                framer.feed(&[*byte]);
                let frame = framer.try_next();
                if i + 1 == stanza.len() { assert_eq!(frame.as_deref(), Some(stanza.as_str())); }
                else { assert!(frame.is_none()); }
                assert!(framer.error().is_none());
            }
            assert!(framer.scanner.steps <= stanza.len() * 4);
        }
    }

    #[test]
    fn stream_prefix_is_preserved_through_close()
    {
        let stream = "<?xml version='1.0'?><s:stream xmlns:s='http://etherx.jabber.org/streams' xmlns='jabber:client'><s:features/></s:stream>";
        let mut framer = XmlFramer::new();
        let mut frames = Vec::new();
        for byte in stream.as_bytes() {
            framer.feed(&[*byte]);
            if let Some(frame) = framer.try_next() { frames.push(frame); }
            assert!(framer.error().is_none());
        }
        assert_eq!(frames.len(), 3);
        assert!(crate::stanza::stream::Stream::from_xml(&frames[0]).is_ok());
        assert!(crate::stanza::stream::StreamFeatures::from_xml(&frames[1]).is_ok());
        assert_eq!(frames[2], "</s:stream>");
        assert!(!framer.stream_is_open());
    }

    #[test]
    fn malformed_markup_entities_and_namespace_bindings_are_rejected()
    {
        for stanza in [
            "<message><body>mismatched</message>",
            "<message attribute='literal<angle'/>",
            "<message attribute=unquoted/>",
            "<message attribute='one' attribute='two'/>",
            "<message><body>&undefined;</body></message>",
            "<message><body>&#0;</body></message>",
            "<message attribute='&#0;'/>",
            "<message>bad]]>text</message>",
            "<unknown:message/>",
            "<message xmlns:x='urn:same' xmlns:y='urn:same' x:a='one' y:a='two'/>",
            "<!DOCTYPE message><message/>",
            "\u{000b}<presence/>",
        ] {
            let mut framer = XmlFramer::new_opened();
            framer.feed(stanza.as_bytes());
            assert!(framer.try_next().is_none(), "{stanza}");
            assert!(framer.error().is_some(), "{stanza}");
        }
    }

    #[test]
    fn feff_is_message_text_not_a_stream_encoding_marker() {
        let mut framer = XmlFramer::new();
        framer.feed("\u{feff}<stream:stream>".as_bytes());
        assert!(framer.try_next().is_none());
        assert_eq!(framer.error().map(FramingError::condition), Some("not-well-formed"));
        let mut framer = XmlFramer::new_opened();
        let message = "<message><body>preserve \u{feff} text</body></message>";
        for byte in message.bytes() { framer.feed(&[byte]); }
        assert_eq!(framer.try_next().as_deref(), Some(message));
    }

    #[test]
    fn equivalent_namespace_spellings_cannot_hide_duplicate_expanded_attributes() {
        for stanza in [
            "<message xmlns:p='urn:same' xmlns:q='urn:&#115;ame' p:a='one' q:a='two'/>",
            "<message xmlns:p='urn:a&amp;b' xmlns:q='urn:a&#38;b' p:a='one' q:a='two'/>",
            "<message xmlns:p='urn:a b' xmlns:q='urn:a\tb' p:a='one' q:a='two'/>",
        ] {
            let mut framer = XmlFramer::new_opened();
            framer.feed(stanza.as_bytes());
            assert!(framer.try_next().is_none(), "duplicate expanded attribute accepted: {stanza}");
            assert!(framer.error().is_some());
        }
    }

    #[test]
    fn escaped_protocol_namespaces_resolve_in_headers_and_stanzas() {
        let mut framer = XmlFramer::new();
        framer.feed(b"<s:stream xmlns:s='http://etherx.jabber.org/&#115;treams' xmlns='jabber:&#99;lient' xmlns:x='urn:xmpp:&#115;id:0'><message from='a@b'><body>unchanged &amp; text</body><x:origin-id id='one'/></message>");
        let header = framer.try_next().unwrap();
        assert!(crate::stanza::stream::Stream::from_xml(&header).is_ok());
        let frame = framer.try_next().unwrap();
        let parsed = crate::stanza::chat::IncomingChatMessage::from_xml(&frame).unwrap();
        assert_eq!(parsed.origin_id.as_deref(), Some("one"));
        assert_eq!(parsed.body.as_deref(), Some("unchanged & text"));
        assert!(crate::stanza::xml::empty_element("<sm:a xmlns:sm='urn:xmpp:&#115;m:3' h='1'/>", b"a", b"urn:xmpp:sm:3").is_ok());
    }

    #[test]
    fn invalid_element_attribute_and_header_names_are_rejected() {
        for stanza in [
            "<1message/>", "<message><1body/></message>",
            "<message 1attribute='value'/>", "<message xmlns:1p='urn:test'/>",
            "<message xmlns:p='urn:test' p:1attribute='value'/>",
            "<message xmlns:p='urn:test' p:a:b='value'/>",
            "<message bad?name='value'/>", "<message bad/name='value'/>",
            "<message><\u{0301}combining/></message>",
        ] {
            let mut framer = XmlFramer::new_opened();
            framer.feed(stanza.as_bytes());
            assert!(framer.try_next().is_none(), "accepted invalid XML: {stanza}");
            assert!(framer.error().is_some());
        }
        for header in [
            "<stream:stream 1attribute='value'>",
            "<stream:stream xmlns:1p='urn:test'>",
            "<stream:stream attribute='&undefined;'>",
            "<stream:stream attribute='&#0;'>",
            "<stream:stream attribute='one' attribute='two'>",
        ] {
            let mut framer = XmlFramer::new();
            framer.feed(header.as_bytes());
            assert!(framer.try_next().is_none(), "accepted invalid header: {header}");
            assert!(framer.error().is_some());
        }
    }

    #[test]
    fn unicode_xml_names_and_continuation_characters_remain_valid() {
        for stanza in [
            "<message><名前 属性='value'/></message>",
            "<message xmlns:名='urn:test'><名:élément 名:属性='value'/></message>",
            "<message><a\u{0301}·-_.0/></message>",
            "<message><\u{10000}name/></message>",
        ] {
            let mut framer = XmlFramer::new_opened();
            framer.feed(stanza.as_bytes());
            assert!(framer.try_next().is_some(), "rejected valid XML: {stanza}");
            assert!(framer.error().is_none());
        }
    }

    #[test]
    fn excessive_nesting_and_attributes_are_rejected()
    {
        let mut framer = XmlFramer::new_opened();
        framer.feed(format!("{}{}", "<x>".repeat(129), "</x>".repeat(129)).as_bytes());
        assert!(framer.try_next().is_none());
        assert!(matches!(framer.error(), Some(FramingError::TooLarge)));
        let attributes: String = (0..MAX_ATTRIBUTES + 1).map(|i| format!(" a{i}='value'")).collect();
        let mut framer = XmlFramer::new_opened();
        framer.feed(format!("<message{attributes}/>").as_bytes());
        assert!(framer.try_next().is_none());
        assert!(matches!(framer.error(), Some(FramingError::TooLarge)));
    }

    #[test]
    fn invalid_utf8_and_forbidden_xml_characters_are_errors_not_replacements()
    {
        for bytes in [b"<message><body>bad\xff</body></message>".as_slice(),
            b"<message><body>bad\0</body></message>".as_slice()] {
            let mut framer = XmlFramer::new_opened();
            framer.feed(bytes);
            assert!(framer.try_next().is_none());
            assert!(matches!(framer.error(), Some(FramingError::InvalidText)));
            framer.feed(b"<presence/>");
            assert!(framer.try_next().is_none());
            framer.reset();
            assert!(framer.error().is_none());
        }
    }

    #[test]
    fn oversized_incomplete_headers_and_stanzas_are_bounded_per_frame()
    {
        let mut framer = XmlFramer::new();
        framer.feed(b"<stream:stream attribute='");
        framer.feed(&vec![b'a'; MAX_HEADER_BYTES]);
        assert!(framer.try_next().is_none());
        assert!(matches!(framer.error(), Some(FramingError::TooLarge)));
        let mut framer = XmlFramer::new_opened();
        framer.feed(b"<presence/><message><body>");
        framer.feed(&vec![b'a'; MAX_FRAME_BYTES]);
        // A large combined buffer must not make its valid first frame fail.
        assert_eq!(framer.try_next().unwrap(), "<presence/>");
        assert!(framer.try_next().is_none());
        assert!(matches!(framer.error(), Some(FramingError::TooLarge)));
    }

    #[test]
    fn framed_stanzas_keep_inherited_namespaces_and_local_overrides()
    {
        use quick_xml::{events::Event, name::ResolveResult, NsReader};
        let mut framer = XmlFramer::new();
        framer.feed(b"<stream:stream xmlns='jabber:client' xmlns:stream='http://etherx.jabber.org/streams' xmlns:sid='urn:xmpp:sid:0'><message><sid:stanza-id id='one' by='room@example'/></message><sid:stanza-id xmlns:sid='local-override'/>");
        framer.try_next().unwrap();
        let first = framer.try_next().unwrap();
        let mut reader = NsReader::from_str(&first);
        let (ns, event) = reader.read_resolved_event().unwrap();
        assert!(matches!(event, Event::Start(_)));
        assert!(matches!(ns, ResolveResult::Bound(ns) if ns.as_ref() == b"jabber:client"));
        let (ns, _) = reader.read_resolved_event().unwrap();
        assert!(matches!(ns, ResolveResult::Bound(ns) if ns.as_ref() == b"urn:xmpp:sid:0"));
        let second = framer.try_next().unwrap();
        let mut reader = NsReader::from_str(&second);
        let (ns, _) = reader.read_resolved_event().unwrap();
        assert!(matches!(ns, ResolveResult::Bound(ns) if ns.as_ref() == b"local-override"));
    }

    #[test]
    fn resetting_stream_discards_inherited_namespace_scope()
    {
        let mut framer = XmlFramer::new();
        framer.feed(b"<stream:stream xmlns:sid='urn:xmpp:sid:0'>");
        framer.try_next().unwrap();
        framer.reset();
        framer.feed(b"<stream:stream><message/>");
        framer.try_next().unwrap();
        assert_eq!(framer.try_next().unwrap(), "<message/>");
    }

    #[test]
    fn framed_messages_inherit_stream_language_and_respect_overrides() {
        use crate::stanza::chat::IncomingChatMessage;
        let mut framer = XmlFramer::new();
        framer.feed(b"<stream:stream xmlns='jabber:client' xmlns:stream='http://etherx.jabber.org/streams' xml:lang='en'><message><body xml:lang='sv'>hej</body><body>Hello</body></message><message xml:lang='sv'><body xml:lang='en'>Hello</body><body>hej</body></message><message xml:lang=''><body xml:lang='en'>Hello</body><body>default</body></message>");
        framer.try_next().unwrap();
        for expected in ["Hello", "hej", "default"] {
            let frame = framer.try_next().unwrap();
            assert_eq!(IncomingChatMessage::from_xml(&frame).unwrap().body.as_deref(), Some(expected));
        }
        framer.reset();
        framer.feed(b"<stream:stream xmlns='jabber:client' xmlns:stream='http://etherx.jabber.org/streams'><message><body xml:lang='sv'>hej</body><body xml:lang='en'>Hello</body></message>");
        framer.try_next().unwrap();
        let frame = framer.try_next().unwrap();
        let attributes = crate::stanza::xml::element(&frame, b"message", b"jabber:client").unwrap();
        assert!(!attributes.iter().any(|(key, _)| key == "xml:lang"));
        assert_eq!(IncomingChatMessage::from_xml(&frame).unwrap().body.as_deref(), Some("hej"));
    }

    #[test]
    fn parse_stream_header()
    {
        let mut framer = XmlFramer::new();
        framer.feed(b"<?xml version='1.0'?><stream:stream from='devsn.se' to='mick@devsn.se' id='abc' xmlns='jabber:client' xmlns:stream='http://etherx.jabber.org/streams'>");
        let frame = framer.try_next().unwrap();
        assert!(frame.contains("stream:stream"));
        assert!(framer.stream_opened);
    }

    #[test]
    fn parse_self_closing_stanza()
    {
        let mut framer = XmlFramer::new();
        framer.feed(b"<?xml version='1.0'?><stream:stream from='x' to='y'>");
        framer.try_next(); // consume header

        framer.feed(b"<proceed xmlns='urn:ietf:params:xml:ns:xmpp-tls'/>");
        let frame = framer.try_next().unwrap();
        assert!(frame.contains("proceed"));
    }

    #[test]
    fn parse_features_stanza()
    {
        let mut framer = XmlFramer::new();
        framer.feed(b"<?xml version='1.0'?><stream:stream from='x' to='y'><stream:features xmlns:stream='http://etherx.jabber.org/streams'><starttls xmlns='urn:ietf:params:xml:ns:xmpp-tls'/></stream:features>");
        framer.try_next(); // consume header

        let frame = framer.try_next().unwrap();
        assert!(frame.contains("stream:features"));
        assert!(frame.contains("starttls"));
    }

    #[test]
    fn incomplete_data()
    {
        let mut framer = XmlFramer::new();
        framer.feed(b"<?xml version='1.0'?><stream:stream from='x'");
        assert!(framer.try_next().is_none());

        framer.feed(b" to='y'>");
        assert!(framer.try_next().is_some());
    }

    #[test]
    fn stream_close()
    {
        let mut framer = XmlFramer::new();
        framer.feed(b"<stream:stream from='x' to='y'>");
        framer.try_next(); // consume header

        framer.feed(b"</stream:stream>");
        let frame = framer.try_next().unwrap();
        assert!(frame.contains("</stream:stream>"));
        assert!(!framer.stream_opened);
    }

    #[test]
    fn multiple_stanzas_in_one_feed()
    {
        let mut framer = XmlFramer::new();
        framer.feed(b"<stream:stream from='x' to='y'>");
        framer.try_next();

        framer.feed(b"<message><body>hello</body></message><message><body>world</body></message>");
        let first = framer.try_next().unwrap();
        assert!(first.contains("hello"));
        let second = framer.try_next().unwrap();
        assert!(second.contains("world"));
        assert!(framer.try_next().is_none());
    }

    #[test]
    fn stanza_split_across_feeds()
    {
        let mut framer = XmlFramer::new();
        framer.feed(b"<stream:stream from='x' to='y'>");
        framer.try_next();

        framer.feed(b"<message><bo");
        assert!(framer.try_next().is_none());
        framer.feed(b"dy>hi</body>");
        assert!(framer.try_next().is_none());
        framer.feed(b"</message>");
        let frame = framer.try_next().unwrap();
        assert!(frame.contains("<message><body>hi</body></message>"));
    }

    #[test]
    fn nested_elements()
    {
        let mut framer = XmlFramer::new();
        framer.feed(b"<stream:stream from='x' to='y'>");
        framer.try_next();

        framer.feed(b"<iq type='result'><query><item jid='a@b'/><item jid='c@d'/></query></iq>");
        let frame = framer.try_next().unwrap();
        assert!(frame.starts_with("<iq"));
        assert!(frame.ends_with("</iq>"));
        assert!(frame.contains("<item jid='a@b'/>"));
        assert!(frame.contains("<item jid='c@d'/>"));
    }

    #[test]
    fn self_closing_at_depth()
    {
        let mut framer = XmlFramer::new();
        framer.feed(b"<stream:stream from='x' to='y'>");
        framer.try_next();

        framer.feed(b"<presence><show/><status/></presence>");
        let frame = framer.try_next().unwrap();
        assert_eq!(frame, "<presence><show/><status/></presence>");
    }

    #[test]
    fn quoted_angle_bracket_in_attribute()
    {
        let mut framer = XmlFramer::new();
        framer.feed(b"<stream:stream from='x' to='y'>");
        framer.try_next();

        framer.feed(b"<message to='user@host' body='a&gt;b'><body>test</body></message>");
        let frame = framer.try_next().unwrap();
        assert!(frame.contains("test"));
    }

    #[test]
    fn whitespace_between_stanzas()
    {
        let mut framer = XmlFramer::new();
        framer.feed(b"<stream:stream from='x' to='y'>");
        framer.try_next();

        framer.feed(b"   \n\t  <presence/>  \n  <presence type='away'/>");
        let first = framer.try_next().unwrap();
        assert_eq!(first, "<presence/>");
        let second = framer.try_next().unwrap();
        assert_eq!(second, "<presence type='away'/>");
    }

    #[test]
    fn empty_feed()
    {
        let mut framer = XmlFramer::new();
        framer.feed(b"");
        assert!(framer.try_next().is_none());
    }

    #[test]
    fn only_whitespace_feed()
    {
        let mut framer = XmlFramer::new();
        framer.feed(b"<stream:stream from='x' to='y'>");
        framer.try_next();

        framer.feed(b"   \n\t  ");
        assert!(framer.try_next().is_none());
    }

    #[test]
    fn reset_and_new_stream()
    {
        let mut framer = XmlFramer::new();
        framer.feed(b"<stream:stream from='x' to='y'>");
        framer.try_next();
        assert!(framer.stream_opened);

        framer.feed(b"<proceed xmlns='urn:ietf:params:xml:ns:xmpp-tls'/>");
        framer.try_next();

        // Simulate TLS upgrade: reset and start fresh
        framer.reset();
        assert!(!framer.stream_opened);
        assert!(framer.try_next().is_none());

        // New stream over TLS
        framer.feed(b"<?xml version='1.0'?><stream:stream from='x' to='y'>");
        let header = framer.try_next().unwrap();
        assert!(header.contains("stream:stream"));
        assert!(framer.stream_opened);

        framer.feed(b"<stream:features xmlns:stream='http://etherx.jabber.org/streams'><mechanisms xmlns='urn:ietf:params:xml:ns:xmpp-sasl'><mechanism>SCRAM-SHA-1</mechanism></mechanisms></stream:features>");
        let features = framer.try_next().unwrap();
        assert!(features.contains("SCRAM-SHA-1"));
    }

    #[test]
    fn stream_header_without_xml_declaration()
    {
        let mut framer = XmlFramer::new();
        framer.feed(b"<stream:stream from='x' to='y' xmlns='jabber:client'>");
        let frame = framer.try_next().unwrap();
        assert!(frame.contains("stream:stream"));
        assert!(framer.stream_opened);
    }

    #[test]
    fn restricted_xml_is_rejected_even_when_fragmented_between_stanzas() {
        for markup in ["<!-- comment -->", "<?instruction data?>", "<!DOCTYPE message>"] {
            for nested in [false, true] {
                let stanza = if nested { format!("<message>{markup}<body>text</body></message>") } else { format!("{markup}<message/>") };
                let mut framer = XmlFramer::new_opened();
                for byte in stanza.bytes() {
                    framer.feed(&[byte]);
                    assert!(framer.try_next().is_none(), "accepted {stanza}");
                    if framer.error().is_some() { break; }
                }
                assert_eq!(framer.error().map(FramingError::condition), Some("restricted-xml"), "{stanza}");
            }
            let mut framer = XmlFramer::new();
            framer.feed(format!("{markup}<stream:stream>").as_bytes());
            assert!(framer.try_next().is_none());
            assert_eq!(framer.error().map(FramingError::condition), Some("restricted-xml"));
        }
    }

    #[test]
    fn xml_declarations_require_xml_one_and_utf8_with_correct_placement() {
        for declaration in ["<?xml version='1.0'?>", "<?xml version=\"1.0\" encoding=\"uTf-8\"?>"] {
            let mut framer = XmlFramer::new();
            for byte in format!("{declaration}<stream:stream>").bytes() {
                framer.feed(&[byte]);
                if let Some(header) = framer.try_next() { assert!(header.contains("stream:stream")); }
                assert!(framer.error().is_none(), "{declaration}");
            }
            assert!(framer.stream_is_open());
        }
        for (declaration, condition) in [
            ("<?xml version='1.1'?>", "not-well-formed"),
            ("<?xml?>", "restricted-xml"),
            ("<?xml encoding='UTF-8'?>", "not-well-formed"),
            ("<?xml version='1.0' encoding='UTF-16'?>", "unsupported-encoding"),
            ("<?xml version='1.0' standalone='no'?>", "restricted-xml"),
            ("<?xml version='1.0' standalone='yes'?>", "restricted-xml"),
            ("<?xml version='1.0' extra='value'?>", "not-well-formed"),
            ("<?xml version='1.0' version='1.0'?>", "not-well-formed"),
            ("<?xml version='1.0'?><?xml version='1.0'?>", "not-well-formed"),
            (" \n<?xml version='1.0'?>", "not-well-formed"),
        ] {
            let mut framer = XmlFramer::new();
            framer.feed(format!("{declaration}<stream:stream>").as_bytes());
            assert!(framer.try_next().is_none(), "accepted {declaration}");
            assert_eq!(framer.error().map(FramingError::condition), Some(condition), "{declaration}");
        }
        let mut framer = XmlFramer::new_opened();
        framer.feed(b"<?xml version='1.0'?><message/>");
        assert!(framer.try_next().is_none());
        assert_eq!(framer.error().map(FramingError::condition), Some("restricted-xml"));
    }

    #[test]
    fn stanza_with_cdata()
    {
        let mut framer = XmlFramer::new();
        framer.feed(b"<stream:stream from='x' to='y'>");
        framer.try_next();

        framer.feed(b"<message><body><![CDATA[<not>xml</not><!-- literal --><?literal?>]]></body></message>");
        let frame = framer.try_next().unwrap();
        assert!(frame.contains("<![CDATA[<not>xml</not><!-- literal --><?literal?>]]>"));
    }

    #[test]
    fn multibyte_char_split_across_feeds()
    {
        let mut framer = XmlFramer::new();
        framer.feed(b"<stream:stream from='x' to='y'>");
        framer.try_next();

        // "héllo 😀" encoded as UTF-8, split mid-character across two feeds.
        let stanza = "<message><body>héllo 😀</body></message>".as_bytes();
        let split = 18; // lands in the middle of the multi-byte 'é'
        framer.feed(&stanza[..split]);
        assert!(framer.try_next().is_none());
        framer.feed(&stanza[split..]);

        let frame = framer.try_next().unwrap();
        assert_eq!(frame, "<message><body>héllo 😀</body></message>");
    }

    #[test]
    fn byte_by_byte_stanza()
    {
        let mut framer = XmlFramer::new();
        framer.feed(b"<stream:stream from='x' to='y'>");
        framer.try_next();

        let stanza = b"<presence/>";
        for byte in stanza.iter()
        {
            framer.feed(&[*byte]);
        }
        let frame = framer.try_next().unwrap();
        assert_eq!(frame, "<presence/>");
    }

    #[test]
    fn byte_by_byte_stream_header()
    {
        let mut framer = XmlFramer::new();
        let header = b"<stream:stream from='x' to='y'>";
        for byte in header.iter()
        {
            framer.feed(&[*byte]);
        }
        let frame = framer.try_next().unwrap();
        assert!(frame.contains("stream:stream"));
        assert!(framer.stream_opened);
    }

    #[test]
    fn full_xmpp_negotiation_sequence()
    {
        let mut framer = XmlFramer::new();

        // Server sends stream header + features
        framer.feed(b"<?xml version='1.0'?><stream:stream xmlns='jabber:client' xmlns:stream='http://etherx.jabber.org/streams' from='example.com' to='user@example.com' id='abc123' version='1.0'>");
        let header = framer.try_next().unwrap();
        assert!(header.contains("id='abc123'"));

        framer.feed(b"<stream:features xmlns:stream='http://etherx.jabber.org/streams'><starttls xmlns='urn:ietf:params:xml:ns:xmpp-tls'/><mechanisms xmlns='urn:ietf:params:xml:ns:xmpp-sasl'><mechanism>SCRAM-SHA-1</mechanism></mechanisms></stream:features>");
        let features = framer.try_next().unwrap();
        assert!(features.contains("starttls"));
        assert!(features.contains("SCRAM-SHA-1"));

        // Server sends <proceed/>
        framer.feed(b"<proceed xmlns='urn:ietf:params:xml:ns:xmpp-tls'/>");
        let proceed = framer.try_next().unwrap();
        assert!(proceed.contains("proceed"));

        // TLS upgrade: reset
        framer.reset();

        // Post-TLS stream
        framer.feed(b"<?xml version='1.0'?><stream:stream xmlns='jabber:client' xmlns:stream='http://etherx.jabber.org/streams' from='example.com' id='def456' version='1.0'>");
        let header2 = framer.try_next().unwrap();
        assert!(header2.contains("def456"));

        framer.feed(b"<stream:features xmlns:stream='http://etherx.jabber.org/streams'><mechanisms xmlns='urn:ietf:params:xml:ns:xmpp-sasl'><mechanism>SCRAM-SHA-1</mechanism></mechanisms></stream:features>");
        let features2 = framer.try_next().unwrap();
        assert!(features2.contains("mechanisms"));

        // SASL challenge/success
        framer.feed(b"<challenge xmlns='urn:ietf:params:xml:ns:xmpp-sasl'>cj1meW</challenge>");
        let challenge = framer.try_next().unwrap();
        assert!(challenge.contains("challenge"));

        framer.feed(b"<success xmlns='urn:ietf:params:xml:ns:xmpp-sasl'>dj1w</success>");
        let success = framer.try_next().unwrap();
        assert!(success.contains("success"));

        // Post-auth stream reset
        framer.reset();

        // Post-auth stream
        framer.feed(b"<?xml version='1.0'?><stream:stream xmlns='jabber:client' xmlns:stream='http://etherx.jabber.org/streams' from='example.com' id='ghi789' version='1.0'>");
        let header3 = framer.try_next().unwrap();
        assert!(header3.contains("ghi789"));

        framer.feed(b"<stream:features xmlns:stream='http://etherx.jabber.org/streams'><bind xmlns='urn:ietf:params:xml:ns:xmpp-bind'/></stream:features>");
        let features3 = framer.try_next().unwrap();
        assert!(features3.contains("bind"));

        // Bind result
        framer.feed(b"<iq type='result' id='bind_1'><bind xmlns='urn:ietf:params:xml:ns:xmpp-bind'><jid>user@example.com/resource</jid></bind></iq>");
        let bind = framer.try_next().unwrap();
        assert!(bind.contains("user@example.com/resource"));
    }

    #[test]
    fn incomplete_closing_tag()
    {
        let mut framer = XmlFramer::new();
        framer.feed(b"<stream:stream from='x' to='y'>");
        framer.try_next();

        framer.feed(b"<message><body>hi</body></messa");
        assert!(framer.try_next().is_none());
        framer.feed(b"ge>");
        let frame = framer.try_next().unwrap();
        assert!(frame.contains("hi"));
    }

    #[test]
    fn incomplete_self_closing_tag()
    {
        let mut framer = XmlFramer::new();
        framer.feed(b"<stream:stream from='x' to='y'>");
        framer.try_next();

        framer.feed(b"<presence/");
        assert!(framer.try_next().is_none());
        framer.feed(b">");
        let frame = framer.try_next().unwrap();
        assert_eq!(frame, "<presence/>");
    }

    #[test]
    fn stream_close_incomplete()
    {
        let mut framer = XmlFramer::new();
        framer.feed(b"<stream:stream from='x' to='y'>");
        framer.try_next();

        framer.feed(b"</stream:stre");
        assert!(framer.try_next().is_none());
        framer.feed(b"am>");
        let frame = framer.try_next().unwrap();
        assert!(frame.contains("</stream:stream>"));
        assert!(!framer.stream_opened);
    }

    #[test]
    fn stanza_after_stream_close_requires_new_stream()
    {
        let mut framer = XmlFramer::new();
        framer.feed(b"<stream:stream from='x' to='y'>");
        framer.try_next();

        framer.feed(b"</stream:stream>");
        framer.try_next();
        assert!(!framer.stream_opened);

        // Without a new stream:stream header, stanzas should not be parsed as stanzas
        // (the framer is back in header-finding mode)
        framer.feed(b"<presence/>");
        assert!(framer.try_next().is_none()); // not a stream:stream header
    }
}
