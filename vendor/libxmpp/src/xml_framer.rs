pub struct XmlFramer
{
    buffer: String,
    stream_opened: bool,
}

impl XmlFramer
{
    pub fn new() -> Self
    {
        return Self { buffer: String::new(), stream_opened: false };
    }

    pub fn new_opened() -> Self
    {
        return Self { buffer: String::new(), stream_opened: true };
    }

    pub fn feed(&mut self, data: &[u8])
    {
        self.buffer.push_str(&String::from_utf8_lossy(data));
    }

    pub fn reset(&mut self)
    {
        self.buffer.clear();
        self.stream_opened = false;
    }

    /// Try to extract the next frame from the buffer.
    /// Before the stream is opened, this extracts the `<stream:stream ...>` header.
    /// After the stream is opened, this extracts complete top-level stanzas.
    pub fn try_next(&mut self) -> Option<String>
    {
        self.skip_whitespace();
        if self.buffer.is_empty()
        {
            return None;
        }

        if !self.stream_opened
        {
            self.try_stream_header()
        }
        else
        {
            self.try_stanza()
        }
    }

    fn skip_whitespace(&mut self)
    {
        let trimmed = self.buffer.trim_start();
        let ws_len = self.buffer.len() - trimmed.len();

        if ws_len > 0
        {
            self.buffer = self.buffer[ws_len..].to_string();
        }
    }

    fn try_stream_header(&mut self) -> Option<String>
    {
        let bytes = self.buffer.as_bytes();
        let len = bytes.len();
        let mut pos = 0;

        // Skip processing instructions (<?xml ...?>)
        while pos < len
        {
            if pos + 1 < len && bytes[pos] == b'<' && bytes[pos + 1] == b'?'
            {
                match self.buffer[pos..].find("?>")
                {
                    Some(end) => pos += end + 2,
                    None => return None,
                }
                while pos < len && bytes[pos].is_ascii_whitespace()
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

        if !self.buffer[pos..].starts_with("<stream:stream")
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
                if bytes[i] == quote_char
                {
                    in_quote = false;
                }
            }
            else if bytes[i] == b'"' || bytes[i] == b'\''
            {
                in_quote = true;
                quote_char = bytes[i];
            }
            else if bytes[i] == b'>'
            {
                let end = i + 1;
                let header = self.buffer[..end].to_string();
                self.buffer = self.buffer[end..].to_string();
                self.stream_opened = true;

                return Some(header);
            }

            i += 1;
        }

        return None;
    }

    fn try_stanza(&mut self) -> Option<String>
    {
        if self.buffer.is_empty()
        {
            return None;
        }

        // Check for stream close
        if self.buffer.starts_with("</stream:stream")
        {
            match self.buffer.find('>')
            {
                Some(end) =>
                {
                    let frame = self.buffer[..end + 1].to_string();
                    self.buffer = self.buffer[end + 1..].to_string();
                    self.stream_opened = false;

                    return Some(frame);
                }
                None => return None,
            }
        }

        let bytes = self.buffer.as_bytes();
        let len = bytes.len();
        let mut i = 0;
        let mut depth = 0i32;

        while i < len
        {
            if bytes[i] != b'<'
            {
                i += 1;
                continue;
            }

            if i + 1 >= len
            {
                return None;
            }

            if bytes[i + 1] == b'/'
            {
                // Closing tag
                match self.buffer[i..].find('>')
                {
                    Some(end) =>
                    {
                        depth -= 1;
                        i += end + 1;
                        if depth == 0
                        {
                            let frame = self.buffer[..i].to_string();
                            self.buffer = self.buffer[i..].to_string();

                            return Some(frame);
                        }
                    }
                    None => return None,
                }
            }
            else if bytes[i + 1] == b'?'
            {
                // Processing instruction
                match self.buffer[i..].find("?>")
                {
                    Some(end) => i += end + 2,
                    None => return None,
                }
            }
            else if bytes[i + 1] == b'!'
            {
                // Comment or CDATA
                if self.buffer[i..].starts_with("<!--")
                {
                    match self.buffer[i..].find("-->")
                    {
                        Some(end) => i += end + 3,
                        None => return None,
                    }
                }
                else if self.buffer[i..].starts_with("<![CDATA[")
                {
                    match self.buffer[i..].find("]]>")
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
                let (tag_end, self_closing) = match self.scan_tag(i)
                {
                    Some(v) => v,
                    None => return None,
                };

                if self_closing
                {
                    if depth == 0
                    {
                        let frame = self.buffer[..tag_end].to_string();
                        self.buffer = self.buffer[tag_end..].to_string();
                        return Some(frame);
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

    /// Scan a tag starting at `start` (which points to '<').
    /// Returns (byte position after '>', is_self_closing).
    fn scan_tag(&self, start: usize) -> Option<(usize, bool)>
    {
        let bytes = self.buffer.as_bytes();
        let len = bytes.len();
        let mut i = start + 1;
        let mut in_quote = false;
        let mut quote_char = b'"';

        while i < len
        {
            if in_quote
            {
                if bytes[i] == quote_char
                {
                    in_quote = false;
                }
            }
            else if bytes[i] == b'"' || bytes[i] == b'\''
            {
                in_quote = true;
                quote_char = bytes[i];
            }
            else if bytes[i] == b'>'
            {
                let self_closing = i > 0 && bytes[i - 1] == b'/';
                return Some((i + 1, self_closing));
            }
            i += 1;
        }

        return None;
    }
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
