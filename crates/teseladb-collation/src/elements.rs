//! Collation elements of a string in NFD (UTS #10, step S2, as ICU 57.1 matches): at each position
//! the longest contiguous match, then the unblocked non-starters after it that extend the match
//! into a longer contraction (S2.1.1 to S2.1.3), which are taken out of the string; a code point
//! that maps to nothing gets its implicit element.
//!
//! Mappings live in tables, one per context (ICU's prefixes, `L|·`) and one without. At each
//! position the tables whose context the text before ends with are tried, longest context first,
//! then the one without; the first with a real match for the position wins, and a discontiguous
//! extension only grows a real match of that same table. The text before is the text as it came,
//! also where an earlier contraction consumed it. Generic over the mappings, so that the root, a
//! tailoring over it and the tailoring builder share one matching algorithm.

/// Where collation elements come from.
pub trait Mappings {
    /// The element type.
    type Element: Copy;
    /// The elements of the string `cps` in the table of `context` (empty: no context).
    fn lookup(&self, context: &[u32], cps: &[u32]) -> Option<&[Self::Element]>;
    /// Whether `cps` starts a longer string of the table of `context`.
    fn starts(&self, context: &[u32], cps: &[u32]) -> bool;
    /// The contexts of the strings starting with `cp`, longest first.
    fn contexts(&self, cp: u32) -> Vec<&[u32]>;
    /// The canonical combining class of a code point.
    fn combining_class(&self, cp: u32) -> u8;
    /// The element of a code point that is not listed.
    fn implicit(&self, cp: u32) -> Self::Element;
    /// The value of a decimal digit that `numericOrdering` reads as part of a number.
    fn digit(&self, cp: u32) -> Option<u8>;
    /// The element of a number's primary `p` (common secondary and tertiary).
    fn numeric(&self, p: u32) -> Self::Element;
    /// The first primary of the numbers.
    fn numeric_primary(&self) -> u32;
}

/// The primaries of a number given by its digits, at most 254 of them without leading zeros
/// (ICU's `appendNumericSegmentCEs`): small numbers in two bytes, then three, four, and beyond
/// one million the count of digit pairs and the pairs.
fn numeric_segment(base: u32, digits: &[u8], out: &mut Vec<u32>) {
    if digits.len() <= 7 {
        let mut value = digits.iter().fold(0u32, |v, d| v * 10 + u32::from(*d));
        if value < 74 {
            out.push(base | ((2 + value) << 16));
            return;
        }
        value -= 74;
        if value < 40 * 254 {
            out.push(base | ((76 + value / 254) << 16) | ((2 + value % 254) << 8));
            return;
        }
        value -= 40 * 254;
        if value < 16 * 254 * 254 {
            let mut p = base | (2 + value % 254);
            value /= 254;
            p |= (2 + value % 254) << 8;
            value /= 254;
            out.push(p | ((116 + value % 254) << 16));
            return;
        }
    }
    let pairs = u32::try_from(digits.len().div_ceil(2)).expect("at most 127 pairs");
    let mut primary = base | ((132 - 4 + pairs) << 16);
    let mut len = digits.len();
    while digits[len - 1] == 0 && digits[len - 2] == 0 {
        len -= 2;
    }
    let (mut pair, mut pos) = if len % 2 == 1 {
        (u32::from(digits[0]), 1)
    } else {
        (u32::from(digits[0]) * 10 + u32::from(digits[1]), 2)
    };
    pair = 11 + 2 * pair;
    let mut shift = 8;
    while pos < len {
        if shift == 0 {
            out.push(primary | pair);
            primary = base;
            shift = 16;
        } else {
            primary |= pair << shift;
            shift -= 8;
        }
        pair = 11 + 2 * (u32::from(digits[pos]) * 10 + u32::from(digits[pos + 1]));
        pos += 2;
    }
    out.push(primary | ((pair - 1) << shift));
}

/// The primaries of a run of digits (ICU's `appendNumericCEs`): leading zeros skipped, in
/// segments of at most 254 digits.
fn numeric_primaries(base: u32, digits: &[u8]) -> Vec<u32> {
    let mut out = Vec::new();
    let mut pos = 0;
    loop {
        while pos < digits.len() - 1 && digits[pos] == 0 {
            pos += 1;
        }
        let len = (digits.len() - pos).min(254);
        numeric_segment(base, &digits[pos..pos + len], &mut out);
        pos += len;
        if pos == digits.len() {
            return out;
        }
    }
}

/// The longest match at `i` in the table of `context`: its length, its elements and the
/// positions of the non-starters it took out of order.
fn match_in<'m, M: Mappings>(
    m: &'m M,
    context: &[u32],
    w: &[u32],
    i: usize,
) -> Option<(usize, &'m [M::Element], Vec<usize>)> {
    let mut best = m.lookup(context, &w[i..=i]).map(|ces| (1, ces));
    let mut k = 2;
    while i + k <= w.len() && m.starts(context, &w[i..i + k - 1]) {
        if let Some(ces) = m.lookup(context, &w[i..i + k]) {
            best = Some((k, ces));
        }
        k += 1;
    }
    let (len, mut found) = best?;
    if w.get(i + len).is_none_or(|c| m.combining_class(*c) == 0) {
        return Some((len, found, Vec::new()));
    }
    // Unblocked non-starters after it that make a longer contraction.
    let mut matched = w[i..i + len].to_vec();
    let mut taken = Vec::new();
    let mut skipped_class = 0;
    for (j, &c) in w.iter().enumerate().skip(i + len) {
        let class = m.combining_class(c);
        if class == 0 {
            break;
        }
        if skipped_class < class {
            matched.push(c);
            if let Some(ces) = m.lookup(context, &matched) {
                found = ces;
                taken.push(j);
                continue;
            }
            matched.pop();
        }
        skipped_class = skipped_class.max(class);
    }
    Some((len, found, taken))
}

/// The collation elements of `cps`, already in NFD.
pub fn elements<M: Mappings>(m: &M, cps: &[u32]) -> Vec<M::Element> {
    elements_numeric(m, cps, false)
}

/// The collation elements of `cps`, already in NFD; with `numeric`, each run of decimal digits
/// becomes the elements of its number (`numericOrdering`).
pub fn elements_numeric<M: Mappings>(m: &M, cps: &[u32], numeric: bool) -> Vec<M::Element> {
    let mut w = cps.to_vec();
    // Where each code point of `w` was in `cps`, for the contexts.
    let mut at: Vec<usize> = (0..cps.len()).collect();
    let mut out = Vec::with_capacity(w.len());
    let mut i = 0;
    while i < w.len() {
        if numeric && m.digit(w[i]).is_some() {
            let digits: Vec<u8> = w[i..].iter().map_while(|c| m.digit(*c)).collect();
            out.extend(
                numeric_primaries(m.numeric_primary(), &digits).into_iter().map(|p| m.numeric(p)),
            );
            i += digits.len();
            continue;
        }
        let before = &cps[..at[i]];
        let contexts = m.contexts(w[i]);
        let tables = contexts.iter().copied().filter(|c| before.ends_with(c)).chain([&[][..]]);
        match tables.into_iter().find_map(|context| match_in(m, context, &w, i)) {
            Some((len, ces, taken)) => {
                out.extend_from_slice(ces);
                for j in taken.into_iter().rev() {
                    w.remove(j);
                    at.remove(j);
                }
                i += len;
            }
            None => {
                out.push(m.implicit(w[i]));
                i += 1;
            }
        }
    }
    out
}
