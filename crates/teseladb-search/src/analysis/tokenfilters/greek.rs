//! The stemmer of the Greek analyzer (the built-in `greek` analyzer): the rule-sets of G. Ntais, «Development
//! of a Stemmer for the Greek Language» (master's thesis, Stockholm University and KTH, 2006,
//! section 3.2, http://people.dsv.su.se/~hercules/papers/Ntais_greek_stemmer_thesis_final.pdf),
//! with the suffixes the thesis lists. The thesis gives only the first entries of each list of
//! stems and endings that keep a suffix; the lists below were deduced from what the reference search server 1.70.6
//! gives for 70 457 stems (the beginnings of the vocabulary words) before each suffix (D-051),
//! and the word endings of rule 0 from the endings cut the same way after any beginning. Equal
//! to the reference search server on the 40 000 words of the fixture and on 399 975 of 399 984 synthetic words.
//!
//! The words come lower-cased by [`greek_lowercase`]: without tonos or dialytika and with final
//! sigma as `σ`. Words of fewer than 4 letters are kept.

/// Endings after which rule 1 does not put back αδ.
const L1: &[&str] = &["γιαγι", "εθερ", "θει", "κυρ", "μαμ", "μαν", "μπαμπ", "νταντ", "οκ", "πατερ"];

/// Endings after which rule 2 puts back what it removed.
const L2: &[&str] = &["γηπ", "δαπ", "εμπ", "ιπ", "κρασπ", "μιλ", "οπ", "υπ"];

/// Endings after which rule 3 puts back what it removed.
const L3: &[&str] =
    &["αρκ", "βελ", "λιχ", "λουλ", "πεταλ", "σ", "σκ", "σπ", "τραγ", "φε", "φλ", "φρ", "χν"];

/// Stems after which rule 4 puts back what it removed.
const E4: &[&str] = &["γαλ", "δ", "ελ", "θ", "ιδ", "ν", "π", "παρ"];

/// Stems after which rule 6 puts back what it removed.
const E6: &[&str] = &[
    "αδ",
    "αλ",
    "αμαν",
    "ανηθ",
    "αντιδ",
    "βρωμ",
    "γερ",
    "ενδ",
    "ηθ",
    "καλλιν",
    "καλπ",
    "καταδ",
    "μουλ",
    "μπαν",
    "μπολ",
    "μποσ",
    "νιτ",
    "πετσ",
    "πιτσ",
    "πρωτοδ",
    "συναδ",
    "τσαμ",
    "υποδ",
    "φυσ",
    "χασ",
];

/// Stems after which rule 7 puts back what it removed.
const E7: &[&str] =
    &["αναπ", "αποθ", "αποκ", "αποστ", "βουβ", "ξεθ", "ουλ", "πεθ", "πικρ", "ποτ", "σιχ", "χ"];

/// Stems after which rule 8 (a) puts back what it removed.
const E8A: &[&str] = &["τρ", "τσ"];

/// Stems after which rule 8 (b) puts back what it removed.
const E8B: &[&str] = &[
    "αγ",
    "αμερικαν",
    "αμηχ",
    "ανικ",
    "ανοργ",
    "απηγ",
    "απιθ",
    "βασ",
    "βασκ",
    "βετερ",
    "βιομηχ",
    "γ",
    "γαλ",
    "γερ",
    "δεκ",
    "διατ",
    "διαφ",
    "διπλ",
    "ενοργ",
    "ερ",
    "ζωντ",
    "θ",
    "θυσ",
    "ικ",
    "καστ",
    "καστελ",
    "κλιβ",
    "κοπ",
    "λιβ",
    "λιχ",
    "λουθηρ",
    "μ",
    "μαιντ",
    "μελ",
    "μουσουλμ",
    "μωαμεθ",
    "ν",
    "νταβ",
    "ορφ",
    "ουλ",
    "ουρ",
    "π",
    "πελεκ",
    "περηφ",
    "περιτρ",
    "πιθ",
    "πλ",
    "πλατ",
    "πολισ",
    "σ",
    "σιγ",
    "σουλτ",
    "σπ",
    "στεγ",
    "στεφ",
    "ταβ",
    "τετ",
    "τσιγγ",
    "υπερηφ",
    "φ",
    "χ",
];

/// Stems after which rule 9 puts back what it removed.
const E9: &[&str] = &[
    "αδ", "αθ", "αν", "απλ", "βεν", "δ", "εμ", "εναρ", "θ", "θαρρ", "κοπ", "μπορ", "ντρ", "παγ",
    "σκ", "σκελ", "τοκ", "υ",
];

/// Endings after which rule 9 puts back what it removed.
const L9: &[&str] = &[
    "αιν",
    "αιρ",
    "αρκ",
    "βαρ",
    "βολ",
    "βρ",
    "ενδ",
    "ενθ",
    "ευθ",
    "ευρ",
    "ιαθ",
    "καθ",
    "κθ",
    "νετ",
    "οδ",
    "πον",
    "πυρ",
    "ραθ",
    "ρακαλ",
    "ροθ",
    "ρον",
    "σθ",
    "συν",
    "συνδ",
    "συνθ",
    "σχ",
    "ταθ",
    "τιθ",
    "υπερθ",
    "φορ",
    "χωρ",
    "ωφελ",
];

/// Stems after which rule 12 (a) puts back what it removed.
const E12A: &[&str] = &["ακαταπ", "απ", "ασυμπ", "π", "συμπ"];

/// Stems after which rule 12 (b) puts back what it removed.
const E12B: &[&str] = &["αλ", "αρ", "εκτελ", "ζ", "μ", "νισ", "ξ", "παρακαλ", "προ"];

/// Stems after which rule 13 puts back what it removed.
const E13: &[&str] = &["διαθ", "θ", "προσθ", "συνθ"];

/// Endings after which rule 13 puts back what it removed.
const L13: &[&str] = &["ακαταθ", "ναρθ", "οθ", "πιθ", "σκουλ", "σφ"];

/// Stems after which rule 14 puts back what it removed.
const E14: &[&str] = &["αγκ", "αιθ", "αμ", "ανηκ", "ενδιαφερ", "λ", "λεχ", "μ", "μεδ", "πατ", "ρ"];

/// Endings after which rule 14 puts back what it removed.
const L14: &[&str] = &[
    "ανταχ",
    "αρμακ",
    "βλεπ",
    "εκλιπ",
    "κυματ",
    "λαχ",
    "ληγ",
    "μαλλ",
    "ναρρ",
    "ομ",
    "πρωτ",
    "σαζ",
    "σποζ",
    "φαγ",
    "φρυδ",
];

/// Stems after which rule 15 (a) puts back what it removed.
const E15A: &[&str] = &[
    "αμαλ",
    "απερ",
    "αποτ",
    "ασπ",
    "αφ",
    "αχαρ",
    "διατ",
    "εν",
    "επιτ",
    "λ",
    "μ",
    "ναυ",
    "ξεφ",
    "ολλ",
    "ομοτ",
    "ουρ",
    "π",
    "προστ",
    "ρ",
    "συμπ",
    "συντ",
    "τ",
    "τρ",
    "χαρ",
];

/// Endings after which rule 15 (a) puts back what it removed.
const L15A: &[&str] = &[
    "αλλ",
    "ανυπ",
    "αολλ",
    "δολλ",
    "ελλ",
    "ιλλ",
    "ιμοστ",
    "λλ",
    "λοχ",
    "νεοπ",
    "νολλ",
    "οφ",
    "πελ",
    "πολλ",
    "πρ",
    "ρπ",
    "σμην",
    "σφ",
    "τολλ",
    "υλλ",
    "υποτ",
    "φρ",
    "χολλ",
    "χορτ",
    "ωποπ",
];

/// Endings after which rule 15 does not put back αγ.
const L15B: &[&str] = &["κολλ"];

/// Stems after which rule 16 puts back what it removed.
const E16: &[&str] = &["δωδεκαν", "επταν", "ν", "χερσον"];

/// Stems after which rule 17 puts back what it removed.
const E17: &[&str] = &["αειμν", "απλ", "ασβ", "αχρ", "ευχρ", "σβ", "χρ"];

/// Stems after which rule 18 puts back what it removed.
const E18: &[&str] = &["ν", "ρ", "σπι"];

/// Stems after which rule 19 puts back what it removed.
const E19: &[&str] = &["αζ", "φ", "χ"];

/// Word endings rule 0 rewrites, with what replaces them.
const R0: &[(&str, &str)] = &[
    ("καθεστωτοσ", "καθεστ"),
    ("καθεστωτων", "καθεστ"),
    ("γεγονοτοσ", "γεγον"),
    ("γεγονοτων", "γεγον"),
    ("καθεστωτα", "καθεστ"),
    ("γεγονοτα", "γεγον"),
    ("καθεστωσ", "καθεστ"),
    ("κρεατοσ", "κρε"),
    ("κρεατων", "κρε"),
    ("ολογιου", "ολο"),
    ("ολογιων", "ολο"),
    ("περατοσ", "περ"),
    ("περατων", "περ"),
    ("τερατοσ", "τερ"),
    ("τερατων", "τερ"),
    ("κρεατα", "κρε"),
    ("ολογια", "ολο"),
    ("περατα", "περ"),
    ("σογιου", "σο"),
    ("σογιων", "σο"),
    ("τερατα", "τερ"),
    ("φαγιου", "φα"),
    ("φαγιων", "φα"),
    ("σογια", "σο"),
    ("φαγια", "φα"),
    ("φωτοσ", "φω"),
    ("φωτων", "φω"),
    ("φωτα", "φω"),
    ("φωσ", "φω"),
];

/// The endings of rule 21 (section 3.2 of the thesis), longest first.
const LONG_LIST: &[&str] = &[
    "ιοντουσαν",
    "ιομασταν",
    "ιοσασταν",
    "ιουμαστε",
    "οντουσαν",
    "ηθηκατε",
    "ιεμαστε",
    "ιεσαστε",
    "ιομουνα",
    "ιοσαστε",
    "ιοσουνα",
    "ιουνται",
    "ιουνταν",
    "ομασταν",
    "οσασταν",
    "ουμαστε",
    "ηθειτε",
    "ηθηκαν",
    "ιομουν",
    "ιονταν",
    "ιοσουν",
    "ομουνα",
    "οσαστε",
    "οσουνα",
    "ουνται",
    "ουνταν",
    "ουσατε",
    "αγατε",
    "ηθεισ",
    "ηθουν",
    "ηκατε",
    "ησατε",
    "ησουν",
    "ιεμαι",
    "ιεσαι",
    "ιεται",
    "ιοταν",
    "ιουμα",
    "ομουν",
    "ονται",
    "ονταν",
    "οσουν",
    "ουμαι",
    "ουσαν",
    "αγαν",
    "αμαι",
    "ασαι",
    "αται",
    "ειτε",
    "εσαι",
    "εται",
    "ηδεσ",
    "ηδων",
    "ηθει",
    "ηκαν",
    "ησαν",
    "ησει",
    "ησεσ",
    "ομαι",
    "οταν",
    "αει",
    "εισ",
    "ηθω",
    "ησω",
    "ουν",
    "ουσ",
    "αν",
    "ασ",
    "αω",
    "ει",
    "εσ",
    "ησ",
    "οι",
    "οσ",
    "ου",
    "υσ",
    "ων",
    "α",
    "ε",
    "η",
    "ι",
    "ο",
    "υ",
    "ω",
];

/// The Greek vowels.
const VOWELS: &str = "αεηιουω";
/// The Greek vowels but `υ`.
const VOWELS_NO_Y: &str = "αεηιοω";

/// The Greek lower-case form of `text`: each letter lower-cased, the vowels without tonos or
/// dialytika and final sigma as `σ`.
pub(super) fn greek_lowercase(text: &str) -> String {
    text.chars()
        .flat_map(char::to_lowercase)
        .map(|c| match c {
            'ά' => 'α',
            'έ' => 'ε',
            'ή' => 'η',
            'ί' | 'ϊ' | 'ΐ' => 'ι',
            'ό' => 'ο',
            'ύ' | 'ϋ' | 'ΰ' => 'υ',
            'ώ' => 'ω',
            'ς' => 'σ',
            other => other,
        })
        .collect()
}

/// A word being stemmed.
struct Word(Vec<char>);

impl Word {
    fn len(&self) -> usize {
        self.0.len()
    }

    fn ends(&self, suffix: &str) -> bool {
        let n = suffix.chars().count();
        n <= self.0.len() && self.0[self.0.len() - n..].iter().copied().eq(suffix.chars())
    }

    fn ends_any(&self, suffixes: &[&str]) -> bool {
        suffixes.iter().any(|s| self.ends(s))
    }

    fn is(&self, stems: &[&str]) -> bool {
        stems.iter().any(|s| self.0.iter().copied().eq(s.chars()))
    }

    fn ends_vowel(&self, vowels: &str) -> bool {
        self.0.last().is_some_and(|c| vowels.contains(*c))
    }

    fn drop_end(&mut self, n: usize) {
        let keep = self.0.len() - n;
        self.0.resize(keep, ' ');
    }

    fn add(&mut self, s: &str) {
        self.0.extend(s.chars());
    }

    /// Drops `n` letters and puts back `back` when `keep` holds for what is left.
    fn drop_unless(&mut self, n: usize, back: &str, keep: impl Fn(&Word) -> bool) {
        self.drop_end(n);
        if keep(self) {
            self.add(back);
        }
    }
}

/// The number of letters of the first of `endings` the word has, when it is longer than
/// `more_than`.
fn ending(w: &Word, more_than: usize, endings: &[&str]) -> Option<usize> {
    if w.len() <= more_than {
        return None;
    }
    endings.iter().find(|e| w.ends(e)).map(|e| e.chars().count())
}

/// Rule 0: a few word endings with their own stems.
fn rule0(w: &mut Word) {
    if let Some((from, to)) = R0.iter().find(|(from, _)| w.ends(from)) {
        w.drop_end(from.chars().count());
        w.add(to);
    }
}

/// Rules 1 to 6: `αδες`, `εδες`, `ουδες`, `εως`, `ια` and `ικα` with their other cases.
fn rules_1_to_6(w: &mut Word) {
    if let Some(n) = ending(w, 4, &["αδεσ", "αδων"]) {
        w.drop_unless(n, "αδ", |w| !w.ends_any(L1));
    }
    if let Some(n) = ending(w, 4, &["εδεσ", "εδων"]) {
        w.drop_unless(n, "εδ", |w| w.ends_any(L2));
    }
    if let Some(n) = ending(w, 5, &["ουδεσ", "ουδων"]) {
        w.drop_unless(n, "ουδ", |w| w.ends_any(L3));
    }
    if let Some(n) = ending(w, 3, &["εωσ", "εων"]) {
        w.drop_unless(n, "ε", |w| w.is(E4));
    }
    if let Some(n) = ending(w, 2, &["ια"]).or_else(|| ending(w, 3, &["ιου", "ιων"])) {
        w.drop_unless(n, "ι", |w| w.ends_vowel(VOWELS));
    }
    if let Some(n) = ending(w, 3, &["ικα", "ικο"]).or_else(|| ending(w, 4, &["ικου", "ικων"]))
    {
        w.drop_unless(n, "ικ", |w| w.ends_vowel(VOWELS) || w.is(E6));
    }
}

/// Rule 7: `αμε` and the past endings that end in it.
fn rule7(w: &mut Word) {
    if w.len() == 5 && w.ends("αγαμε") {
        w.drop_end(1);
        return;
    }
    let past = ending(w, 7, &["ηθηκαμε"])
        .or_else(|| ending(w, 6, &["ουσαμε"]))
        .or_else(|| ending(w, 5, &["αγαμε", "ησαμε", "ηκαμε"]));
    if let Some(n) = past {
        w.drop_end(n);
    }
    if let Some(n) = ending(w, 3, &["αμε"]) {
        w.drop_unless(n, "αμ", |w| w.is(E7));
    }
}

/// Rule 8: `ανε` and the past endings that end in it. The length checks apply to the first
/// ending of each group only, as the reference search server does.
fn rule8(w: &mut Word) {
    let past = if w.len() > 8 && w.ends("ιουντανε") {
        Some(8)
    } else if (w.len() > 7 && w.ends("ιοντανε")) || w.ends_any(&["ουντανε", "ηθηκανε"])
    {
        Some(7)
    } else if (w.len() > 6 && w.ends("ιοτανε")) || w.ends_any(&["οντανε", "ουσανε"])
    {
        Some(6)
    } else if (w.len() > 5 && w.ends("αγανε")) || w.ends_any(&["ησανε", "οτανε", "ηκανε"])
    {
        Some(5)
    } else {
        None
    };
    if let Some(n) = past {
        w.drop_unless(n, "αγαν", |w| w.is(E8A));
    }
    if let Some(n) = ending(w, 3, &["ανε"]) {
        w.drop_unless(n, "αν", |w| w.ends_vowel(VOWELS_NO_Y) || w.is(E8B));
    }
}

/// Rules 9 to 12: `ετε`, the participles in `οντας`, `ομαστε` and `εστε`.
fn rules_9_to_12(w: &mut Word) {
    if let Some(n) = ending(w, 5, &["ησετε"]) {
        w.drop_end(n);
    }
    if let Some(n) = ending(w, 3, &["ετε"]) {
        w.drop_unless(n, "ετ", |w| w.is(E9) || w.ends_vowel(VOWELS_NO_Y) || w.ends_any(L9));
    }
    if let Some(n) = ending(w, 5, &["οντασ", "ωντασ"]) {
        w.drop_end(n);
        if w.len() == 3 && w.ends("αρχ") {
            w.add("οντ");
        }
        if w.ends("κρε") {
            w.add("ωντ");
        }
    }
    if let Some(n) = ending(w, 6, &["ομαστε"]).or_else(|| ending(w, 7, &["ιομαστε"])) {
        w.drop_unless(n, "ομαστ", |w| w.len() == 2 && w.ends("ον"));
    }
    if let Some(n) = ending(w, 5, &["ιεστε"]) {
        w.drop_unless(n, "ιεστ", |w| w.is(E12A));
    }
    if let Some(n) = ending(w, 4, &["εστε"]) {
        w.drop_unless(n, "εστ", |w| w.is(E12B));
    }
}

/// Rules 13 to 15: the past `ηθηκα` and `ηκα`, `ουσα` and `αγα` with their other forms.
fn rules_13_to_15(w: &mut Word) {
    if let Some(n) = ending(w, 6, &["ηθηκεσ"]).or_else(|| ending(w, 5, &["ηθηκα", "ηθηκε"]))
    {
        w.drop_end(n);
    }
    if let Some(n) = ending(w, 4, &["ηκεσ"]).or_else(|| ending(w, 3, &["ηκα", "ηκε"])) {
        w.drop_unless(n, "ηκ", |w| w.is(E13) || w.ends_any(L13));
    }
    if let Some(n) = ending(w, 5, &["ουσεσ"]).or_else(|| ending(w, 4, &["ουσα", "ουσε"]))
    {
        w.drop_unless(n, "ουσ", |w| w.is(E14) || w.ends_vowel(VOWELS) || w.ends_any(L14));
    }
    if let Some(n) = ending(w, 4, &["αγεσ"]).or_else(|| ending(w, 3, &["αγα", "αγε"])) {
        w.drop_unless(n, "αγ", |w| (w.is(E15A) || w.ends_any(L15A)) && !w.ends_any(L15B));
    }
}

/// Rules 16 to 20: `ησε`, `ηστε`, `ουνε`, `ουμε` and `ματα`.
fn rules_16_to_20(w: &mut Word) {
    if let Some(n) = ending(w, 4, &["ησου"]).or_else(|| ending(w, 3, &["ησε", "ησα"])) {
        w.drop_unless(n, "ησ", |w| w.is(E16));
    }
    if let Some(n) = ending(w, 4, &["ηστε"]) {
        w.drop_unless(n, "ηστ", |w| w.is(E17));
    }
    if let Some(n) = ending(w, 6, &["ησουνε", "ηθουνε"]).or_else(|| ending(w, 4, &["ουνε"]))
    {
        w.drop_unless(n, "ουν", |w| w.is(E18));
    }
    if let Some(n) = ending(w, 6, &["ησουμε", "ηθουμε"]).or_else(|| ending(w, 4, &["ουμε"]))
    {
        w.drop_unless(n, "ουμ", |w| w.is(E19));
    }
    if ending(w, 5, &["ματων", "ματοσ"]).is_some() {
        w.drop_end(3);
    } else if ending(w, 4, &["ματα"]).is_some() {
        w.drop_end(2);
    }
}

/// Rule 21: the longest ending of the long list of section 3.2 the word is longer than.
fn rule21(w: &mut Word) {
    if let Some(s) = LONG_LIST.iter().find(|s| w.len() > s.chars().count() && w.ends(s)) {
        w.drop_end(s.chars().count());
    }
}

/// Rule 22: the comparative and superlative endings.
fn rule22(w: &mut Word) {
    if w.ends_any(&["εστερ", "εστατ"]) {
        w.drop_end(5);
    } else if w.ends_any(&["οτερ", "οτατ", "υτερ", "υτατ", "ωτερ", "ωτατ"])
    {
        w.drop_end(4);
    }
}

/// The Greek stem of a token lower-cased by [`greek_lowercase`].
pub(super) fn greek_stem(text: &str) -> String {
    let mut w = Word(text.chars().collect());
    if w.len() < 4 {
        return text.to_string();
    }
    let original = w.len();
    rule0(&mut w);
    rules_1_to_6(&mut w);
    rule7(&mut w);
    rule8(&mut w);
    rules_9_to_12(&mut w);
    rules_13_to_15(&mut w);
    rules_16_to_20(&mut w);
    if w.len() == original {
        rule21(&mut w);
    }
    rule22(&mut w);
    w.0.into_iter().collect()
}
