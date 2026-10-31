//! ICU resource bundles in `genrb`'s text format, the subset the collation files of ICU 57.1 use:
//! `name{…}` tables, strings (adjacent `"…"` pieces concatenated, escapes `\uXXXX`, `\UXXXXXXXX`,
//! `\\` and `\"`), quoted keys such as `"%%ALIAS"`, a `:type` after a key (kept out of the key),
//! `//` and `/* */` comments and a leading byte order mark.

/// A resource: a string or a table of named resources, in file order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Res {
    /// A string.
    Str(String),
    /// A table.
    Table(Vec<(String, Res)>),
}

impl Res {
    /// The resource under `key` of a table.
    pub fn get(&self, key: &str) -> Option<&Res> {
        match self {
            Res::Table(entries) => entries.iter().find(|(k, _)| k == key).map(|(_, r)| r),
            Res::Str(_) => None,
        }
    }

    /// The string, if this is one.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Res::Str(s) => Some(s),
            Res::Table(_) => None,
        }
    }
}

/// Text that is not a resource bundle, with the offset of the character.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResError(pub String);

/// The name and table of a bundle.
pub fn parse(text: &str) -> Result<(String, Res), ResError> {
    let mut p = Reader { c: text.trim_start_matches('\u{FEFF}').chars().collect(), pos: 0 };
    p.skip();
    let name = p.key()?;
    let res = p.value()?;
    p.skip();
    if p.pos != p.c.len() {
        return Err(p.err("text after the bundle"));
    }
    Ok((name, res))
}

struct Reader {
    c: Vec<char>,
    pos: usize,
}

impl Reader {
    fn err(&self, what: &str) -> ResError {
        ResError(format!("at {}: {what}", self.pos))
    }

    fn at(&self, s: &str) -> bool {
        let n = s.chars().count();
        self.c.get(self.pos..self.pos + n).is_some_and(|w| w.iter().copied().eq(s.chars()))
    }

    /// Skips spaces and comments.
    fn skip(&mut self) {
        loop {
            while self.c.get(self.pos).is_some_and(|c| c.is_whitespace()) {
                self.pos += 1;
            }
            if self.at("//") {
                while self.c.get(self.pos).is_some_and(|c| *c != '\n') {
                    self.pos += 1;
                }
            } else if self.at("/*") {
                let end = (self.pos + 2..self.c.len().saturating_sub(1))
                    .find(|&i| self.c[i] == '*' && self.c[i + 1] == '/');
                self.pos = end.map_or(self.c.len(), |i| i + 2);
            } else {
                return;
            }
        }
    }

    /// A key: quoted, or up to `{` or a space; a `:type` after it is dropped.
    fn key(&mut self) -> Result<String, ResError> {
        let key = if self.at("\"") {
            self.string()?
        } else {
            let start = self.pos;
            while self
                .c
                .get(self.pos)
                .is_some_and(|c| !c.is_whitespace() && !matches!(c, '{' | '}' | ':' | '"'))
            {
                self.pos += 1;
            }
            self.c[start..self.pos].iter().collect()
        };
        if key.is_empty() {
            return Err(self.err("a missing key"));
        }
        if self.at(":") {
            while self.c.get(self.pos).is_some_and(|c| *c != '{') {
                self.pos += 1;
            }
        }
        self.skip();
        Ok(key)
    }

    /// `{` then strings or entries, then `}`. A quoted text followed by `{` is a key.
    fn value(&mut self) -> Result<Res, ResError> {
        if !self.at("{") {
            return Err(self.err("a '{' expected"));
        }
        self.pos += 1;
        self.skip();
        let mut entries = Vec::new();
        if self.at("\"") {
            let mut s = self.string()?;
            self.skip();
            if self.at("{") {
                entries.push((self.key_named(s)?, self.value()?));
                self.skip();
            } else {
                while self.at("\"") {
                    s.push_str(&self.string()?);
                    self.skip();
                }
                self.close()?;
                return Ok(Res::Str(s));
            }
        }
        while !self.at("}") {
            if self.pos == self.c.len() {
                return Err(self.err("a table without its '}'"));
            }
            let key = self.key()?;
            entries.push((key, self.value()?));
            self.skip();
        }
        self.close()?;
        Ok(Res::Table(entries))
    }

    /// A quoted key already read.
    fn key_named(&self, key: String) -> Result<String, ResError> {
        if key.is_empty() {
            return Err(self.err("a missing key"));
        }
        Ok(key)
    }

    fn close(&mut self) -> Result<(), ResError> {
        if !self.at("}") {
            return Err(self.err("a '}' expected"));
        }
        self.pos += 1;
        Ok(())
    }

    /// A quoted string piece, unescaped.
    fn string(&mut self) -> Result<String, ResError> {
        self.pos += 1;
        let mut out = String::new();
        loop {
            match self.c.get(self.pos).copied() {
                None => return Err(self.err("a string without its end")),
                Some('"') => {
                    self.pos += 1;
                    return Ok(out);
                }
                Some('\\') => {
                    let digits = match self.c.get(self.pos + 1) {
                        Some('u') => 4,
                        Some('U') => 8,
                        Some(&c @ ('\\' | '"')) => {
                            out.push(c);
                            self.pos += 2;
                            continue;
                        }
                        _ => return Err(self.err("an escape genrb data does not use")),
                    };
                    let hex: String = self
                        .c
                        .get(self.pos + 2..self.pos + 2 + digits)
                        .ok_or_else(|| self.err("a short escape"))?
                        .iter()
                        .collect();
                    let c = u32::from_str_radix(&hex, 16)
                        .ok()
                        .and_then(char::from_u32)
                        .ok_or_else(|| self.err("an escape that is not a character"))?;
                    out.push(c);
                    self.pos += 2 + digits;
                }
                Some(c) => {
                    out.push(c);
                    self.pos += 1;
                }
            }
        }
    }
}
