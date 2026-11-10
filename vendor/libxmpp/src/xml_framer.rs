/// Find the first occurrence of `needle` within `haystack`, returning its
/// start offset. Used for byte-oriented scanning of the buffer.
fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize>
{
    if needle.is_empty()
    {
        return Some(0);
    }

    return haystack.windows(needle.len()).position(|w| w == needle);
}

/// Outcome of scanning the buffer for a complete top-level stanza.
enum StanzaEnd
{
    /// A complete stanza ends `end` bytes into the scanned slice.
    Frame(usize),
    /// A `</stream:stream>` close tag ends `end` bytes into the slice.
    StreamClose(usize),
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
}

impl XmlFramer
{
    pub fn new() -> Self
    {
        return Self { buffer: Vec::new(), start: 0, stream_opened: false };
    }

    pub fn new_opened() -> Self
    {
        return Self { buffer: Vec::new(), start: 0, stream_opened: true };
    }

    pub fn feed(&mut self, data: &[u8])
    {
        self.buffer.extend_from_slice(data);
    }

    pub fn reset(&mut self)
    {
        self.buffer.clear();
        self.start = 0;
        self.stream_opened = false;
    }

    /// The unconsumed portion of the buffer.
    fn active(&self) -> &[u8]
    {
        return &self.buffer[self.start..];
    }

    /// Consume `n` bytes from the front of the active region, returning them
    /// decoded as a string. A complete frame is delimited by ASCII `<`/`>`, so
    /// any multi-byte characters inside it are whole by construction.
    fn advance(&mut self, n: usize) -> String
    {
        let end = self.start + n;
        let frame = String::from_utf8_lossy(&self.buffer[self.start..end]).into_owned();
        self.start = end;
        self.compact();

        return frame;
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
        self.skip_whitespace();
        if self.active().is_empty()
        {
            return None;
        }

        if !self.stream_opened
        {
            let end = scan_stream_header(self.active())?;
            self.stream_opened = true;
            return Some(self.advance(end));
        }

        match scan_stanza(self.active())?
        {
            StanzaEnd::Frame(end) => Some(self.advance(end)),
            StanzaEnd::StreamClose(end) =>
            {
                self.stream_opened = false;
                Some(self.advance(end))
            }
        }
    }

    fn skip_whitespace(&mut self)
    {
        let ws_len = self.active().iter().take_while(|b| b.is_ascii_whitespace()).count();
        self.start += ws_len;
    }
}

/// Scan for a `<stream:stream ...>` header. Returns the byte offset one past
/// its closing `>`, or `None` if the header is incomplete or absent.
fn scan_stream_header(buf: &[u8]) -> Option<usize>
{
    let len = buf.len();
    let mut pos = 0;

    // Skip processing instructions (<?xml ...?>)
    while pos < len
    {
        if pos + 1 < len && buf[pos] == b'<' && buf[pos + 1] == b'?'
        {
            match find_bytes(&buf[pos..], b"?>")
            {
                Some(end) => pos += end + 2,
                None => return None,
            }
            while pos < len && buf[pos].is_ascii_whitespace()
            {
                pos += 1;
            }
        }
        else
        {
            break;
        }
    }

    if pos >= len
    {
        return None;
    }

    if !buf[pos..].starts_with(b"<stream:stream")
    {
        return None;
    }

    // Find the closing '>' of the opening tag, handling quoted attributes
    let mut i = pos + 14;
    let mut in_quote = false;
    let mut quote_char = b'"';

    while i < len
    {
        if in_quote
        {
            if buf[i] == quote_char
            {
                in_quote = false;
            }
        }
        else if buf[i] == b'"' || buf[i] == b'\''
        {
            in_quote = true;
            quote_char = buf[i];
        }
        else if buf[i] == b'>'
        {
            return Some(i + 1);
        }

        i += 1;
    }

    return None;
}

/// Scan for one complete top-level stanza (or a stream close tag). Returns
/// `None` if the buffer does not yet hold a complete element.
fn scan_stanza(buf: &[u8]) -> Option<StanzaEnd>
{
    if buf.is_empty()
    {
        return None;
    }

    // Check for stream close
    if buf.starts_with(b"</stream:stream")
    {
        return find_bytes(buf, b">").map(|end| StanzaEnd::StreamClose(end + 1));
    }

    let len = buf.len();
    let mut i = 0;
    let mut depth = 0i32;

    while i < len
    {
        if buf[i] != b'<'
        {
            i += 1;
            continue;
        }

        if i + 1 >= len
        {
            return None;
        }

        if buf[i + 1] == b'/'
        {
            // Closing tag
            match find_bytes(&buf[i..], b">")
            {
                Some(end) =>
                {
                    depth -= 1;
                    i += end + 1;
                    if depth == 0
                    {
                        return Some(StanzaEnd::Frame(i));
                    }
                }
                None => return None,
            }
        }
        else if buf[i + 1] == b'?'
        {
            // Processing instruction
            match find_bytes(&buf[i..], b"?>")
            {
                Some(end) => i += end + 2,
                None => return None,
            }
        }
        else if buf[i + 1] == b'!'
        {
            // Comment or CDATA
            if buf[i..].starts_with(b"<!--")
            {
                match find_bytes(&buf[i..], b"-->")
                {
                    Some(end) => i += end + 3,
                    None => return None,
                }
            }
            else if buf[i..].starts_with(b"<![CDATA[")
            {
                match find_bytes(&buf[i..], b"]]>")
                {
                    Some(end) => i += end + 3,
                    None => return None,
                }
            }
            else
            {
                i += 1;
            }
        }
        else
        {
            // Opening or self-closing tag
            let (tag_end, self_closing) = scan_tag(buf, i)?;

            if self_closing
            {
                if depth == 0
                {
                    return Some(StanzaEnd::Frame(tag_end));
                }

                i = tag_end;
            }
            else
            {
                depth += 1;
                i = tag_end;
            }
        }
    }

    return None;
}

/// Scan a tag in `buf` starting at `start` (which points to '<').
/// Returns (byte position after '>', is_self_closing).
fn scan_tag(buf: &[u8], start: usize) -> Option<(usize, bool)>
{
    let len = buf.len();
    let mut i = start + 1;
    let mut in_quote = false;
    let mut quote_char = b'"';

    while i < len
    {
        if in_quote
        {
            if buf[i] == quote_char
            {
                in_quote = false;
            }
        }
        else if buf[i] == b'"' || buf[i] == b'\''
        {
            in_quote = true;
            quote_char = buf[i];
        }
        else if buf[i] == b'>'
        {
            let self_closing = i > 0 && buf[i - 1] == b'/';
            return Some((i + 1, self_closing));
        }
        i += 1;
    }

    return None;
}

#[cfg(test)]
mod tests
{
    use super::*;

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
        framer.feed(b"<?xml version='1.0'?><stream:stream from='x' to='y'><stream:features><starttls xmlns='urn:ietf:params:xml:ns:xmpp-tls'/></stream:features>");
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

        framer.feed(b"<stream:features><mechanisms xmlns='urn:ietf:params:xml:ns:xmpp-sasl'><mechanism>SCRAM-SHA-1</mechanism></mechanisms></stream:features>");
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
    fn stanza_with_xml_comment()
    {
        let mut framer = XmlFramer::new();
        framer.feed(b"<stream:stream from='x' to='y'>");
        framer.try_next();

        framer.feed(b"<message><!-- a comment --><body>hi</body></message>");
        let frame = framer.try_next().unwrap();
        assert!(frame.contains("<!-- a comment -->"));
        assert!(frame.contains("<body>hi</body>"));
    }

    #[test]
    fn stanza_with_cdata()
    {
        let mut framer = XmlFramer::new();
        framer.feed(b"<stream:stream from='x' to='y'>");
        framer.try_next();

        framer.feed(b"<message><body><![CDATA[<not>xml</not>]]></body></message>");
        let frame = framer.try_next().unwrap();
        assert!(frame.contains("<![CDATA[<not>xml</not>]]>"));
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

        framer.feed(b"<stream:features><starttls xmlns='urn:ietf:params:xml:ns:xmpp-tls'/><mechanisms xmlns='urn:ietf:params:xml:ns:xmpp-sasl'><mechanism>SCRAM-SHA-1</mechanism></mechanisms></stream:features>");
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

        framer.feed(b"<stream:features><mechanisms xmlns='urn:ietf:params:xml:ns:xmpp-sasl'><mechanism>SCRAM-SHA-1</mechanism></mechanisms></stream:features>");
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

        framer.feed(b"<stream:features><bind xmlns='urn:ietf:params:xml:ns:xmpp-bind'/></stream:features>");
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
