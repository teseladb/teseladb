//! Tests of the command-line and configuration-file option parser (long and short options,
//! inline and implicit values, multi-token vectors, positional arguments, defaults, required
//! options, notifiers, help text) and of the option values of the option groups (string maps,
//! switches, selections).
#![cfg(target_os = "linux")]
// This file calls unsafe engine interfaces directly; every `unsafe` block states why it is
// sound in a `// SAFETY:` comment.
#![allow(unsafe_code)]

use std::cell::Cell;
use std::rc::Rc;

use teseladb::engine::core::program_options::bpo::{
    self, OptionDescription, OptionsDescription, PositionalOptionsDescription, ValidationError,
    ValueSemantic, validators,
};
use teseladb::engine::core::sstring::Sstring;
use teseladb::engine::util::program_options::{
    Candidate, OptionGroup, SelectionValue, StringMap, Switch, Unused, Value, ValueHandle,
    get_or_default, read_string_map, validate_string_map,
};

fn args(a: &[&str]) -> Vec<String> {
    a.iter().map(|s| s.to_string()).collect()
}

fn description() -> OptionsDescription {
    let mut d = OptionsDescription::new("Main options");
    d.add_options()
        .flag("help,h", "show help")
        .opt("count,c", bpo::value::<u32>().default_value(3), "how many")
        .opt("name", bpo::value::<String>(), "a name")
        .opt("level", bpo::value::<u8>().implicit_value(9), "a level with an implicit value")
        .opt("verbose", bpo::value::<bool>(), "a boolean")
        .opt("ratio", bpo::value::<f64>().default_value(0.5), "a float")
        .opt("files", bpo::vector_value::<String>().multitoken().composing(), "input files")
        .opt("ids", bpo::vector_value::<u16>().default_value(vec![1]), "ids");
    let mut nested = OptionsDescription::new("Nested");
    nested.add_options().opt("section.key", bpo::value::<Sstring>(), "a nested key");
    d.add(nested);
    d
}

fn parse(d: &OptionsDescription, a: &[&str]) -> Result<bpo::VariablesMap, bpo::Error> {
    let parsed = bpo::command_line_parser(args(a)).options(d).run()?;
    let mut vm = bpo::VariablesMap::new();
    bpo::store(parsed, &mut vm)?;
    bpo::notify(&mut vm)?;
    Ok(vm)
}

#[test]
fn long_short_inline_and_default_values() {
    let d = description();
    let vm = parse(&d, &["-h", "--name=bob", "-c5", "--verbose", "off", "--files", "a", "b", "c"])
        .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(vm.count("help"), 1);
    // A short option takes its value attached.
    assert_eq!(vm.get::<u32>("count"), Some(&5));
    assert_eq!(vm.get::<String>("name").map(String::as_str), Some("bob"));
    assert_eq!(vm.get::<bool>("verbose"), Some(&false));
    assert_eq!(vm.get::<Vec<String>>("files").unwrap(), &args(&["a", "b", "c"]));
    // Defaults fill what was not given, and say so.
    assert_eq!(vm.get::<f64>("ratio"), Some(&0.5));
    assert!(vm.find("ratio").unwrap().defaulted());
    assert!(!vm.find("name").unwrap().defaulted());
    assert_eq!(vm.get::<Vec<u16>>("ids").unwrap(), &vec![1]);
    assert!(vm.find("name").unwrap().value().is::<String>());
    assert_eq!(vm.count("missing"), 0);
    assert!(vm.get::<u32>("name").is_none(), "wrong type");
}

#[test]
fn repeated_single_values_are_refused() {
    let d = description();
    let e = parse(&d, &["--count", "7", "-c5"]).err().expect("count given twice");
    assert!(e.what().contains("more than once"), "{e}");
    assert!(e.to_string().contains("--count"), "{e}");
}

#[test]
fn implicit_values_and_missing_arguments() {
    let d = description();
    let vm = parse(&d, &["--level"]).unwrap();
    assert_eq!(vm.get::<u8>("level"), Some(&9));
    let vm = parse(&d, &["--level", "4"]).unwrap();
    assert_eq!(vm.get::<u8>("level"), Some(&4));
    // An option followed by another option takes its implicit value.
    let vm = parse(&d, &["--level", "--name", "x"]).unwrap();
    assert_eq!(vm.get::<u8>("level"), Some(&9));

    let e = parse(&d, &["--name"]).err().unwrap();
    assert_eq!(e.what(), "the required argument for option '--name' is missing");
    let e = parse(&d, &["--count", "many"]).err().unwrap();
    assert!(e.what().contains("the argument ('many') is invalid"), "{e}");
    let e = parse(&d, &["--verbose", "maybe"]).err().unwrap();
    assert!(e.what().contains("is not a valid boolean"), "{e}");
    let e = parse(&d, &["--ids", "x"]).err().unwrap();
    assert!(e.what().contains("the argument ('x') is invalid"), "{e}");
    let e = parse(&d, &["--help=yes"]).err().unwrap();
    assert_eq!(e.what(), "option 'help' does not take any arguments");
    let e = parse(&d, &["--nope"]).err().unwrap();
    assert_eq!(e.what(), "unrecognised option '--nope'");
    let e = parse(&d, &["-z"]).err().unwrap();
    assert_eq!(e.what(), "unrecognised option '-z'");
    let e = parse(&d, &["stray"]).err().unwrap();
    assert!(e.what().contains("too many positional options"), "{e}");
}

#[test]
fn booleans_accept_the_usual_words() {
    let d = description();
    for (word, value) in [("on", true), ("YES", true), ("1", true), ("no", false), ("0", false)] {
        let vm = parse(&d, &["--verbose", word]).unwrap();
        assert_eq!(vm.get::<bool>("verbose"), Some(&value), "{word}");
    }
}

#[test]
fn positional_arguments_fill_named_options() {
    let mut d = OptionsDescription::new("");
    d.add_options()
        .opt("input", bpo::value::<String>(), "the input")
        .opt("rest", bpo::vector_value::<String>(), "the rest")
        .flag("x", "a flag");
    let mut pos = PositionalOptionsDescription::default();
    pos.add("input", 1).add("rest", -1);
    assert_eq!(pos.name_for_position(0), Some("input"));
    assert_eq!(pos.name_for_position(1), Some("rest"));
    assert_eq!(pos.name_for_position(100), Some("rest"));
    let mut limited = PositionalOptionsDescription::default();
    limited.add("input", 1);
    assert_eq!(limited.name_for_position(1), None);

    let parsed = bpo::command_line_parser(args(&["in.txt", "--x", "r1", "--", "-r2"]))
        .options(&d)
        .positional(&pos)
        .run()
        .unwrap();
    let mut vm = bpo::VariablesMap::new();
    bpo::store(parsed, &mut vm).unwrap();
    assert_eq!(vm.get::<String>("input").map(String::as_str), Some("in.txt"));
    // After "--" an argument that looks like an option is positional.
    assert_eq!(vm.get::<Vec<String>>("rest").unwrap(), &args(&["r1", "-r2"]));
    assert_eq!(vm.count("x"), 1);

    // A parser without descriptions knows no options.
    let e = bpo::command_line_parser(args(&["--a"])).run().err().unwrap();
    assert_eq!(e.what(), "unrecognised option '--a'");
}

#[test]
fn required_options_and_notifiers() {
    let seen = Rc::new(Cell::new(0u32));
    let s = seen.clone();
    let mut d = OptionsDescription::new("req");
    d.add_options()
        .opt("must", bpo::value::<u32>().required().notifier(move |v| s.set(*v)), "required")
        .opt("list", bpo::vector_value::<u32>().required(), "a required list");
    let e = parse(&d, &["--list", "1"]).err().unwrap();
    assert_eq!(e.what(), "the option '--must' is required but missing");
    parse(&d, &["--must", "12", "--list", "1"]).unwrap();
    assert_eq!(seen.get(), 12);

    let sem = bpo::value::<u32>().required();
    assert!(sem.is_required());
    assert!(sem.takes_value());
    assert!(!sem.is_composing());
    assert!(!sem.is_multitoken());
    let v = bpo::vector_value::<u32>();
    assert!(v.is_composing());
    assert!(!v.is_multitoken());
    assert!(v.implicit_value().is_none());
    assert!(ValueSemantic::default_value(&v).is_none());
}

#[test]
fn values_already_stored_are_kept() {
    let d = description();
    let mut vm = bpo::VariablesMap::new();
    let first = bpo::command_line_parser(args(&["--name", "first"])).options(&d).run().unwrap();
    bpo::store(first, &mut vm).unwrap();
    let second = bpo::command_line_parser(args(&["--name", "second", "--count", "1"]))
        .options(&d)
        .run()
        .unwrap();
    bpo::store(second, &mut vm).unwrap();
    assert_eq!(vm.get::<String>("name").map(String::as_str), Some("first"));
    // A defaulted value is replaced.
    assert_eq!(vm.get::<u32>("count"), Some(&1));
}

#[test]
fn config_files_use_sections_as_prefixes() {
    let d = description();
    let text = "# a comment\nname = alice  # trailing\n\n[section]\nkey = value\n";
    let parsed = bpo::parse_config_file(text.as_bytes(), &d).unwrap();
    let mut vm = bpo::VariablesMap::new();
    bpo::store(parsed, &mut vm).unwrap();
    assert_eq!(vm.get::<String>("name").map(String::as_str), Some("alice"));
    assert_eq!(vm.get::<Sstring>("section.key").map(Sstring::as_str), Some("value"));

    let e = bpo::parse_config_file("no equals sign\n".as_bytes(), &d).err().unwrap();
    assert!(e.what().starts_with("invalid line in config file"), "{e}");
    let e = bpo::parse_config_file("other = 1\n".as_bytes(), &d).err().unwrap();
    assert_eq!(e.what(), "unrecognised option 'other'");
}

#[test]
fn help_text_lists_every_option() {
    let d = description();
    let text = d.to_string();
    assert!(text.starts_with("Main options:\n"), "{text}");
    assert!(text.contains("  -h [ --help ]"));
    assert!(text.contains("  -c [ --count ] arg (=3)"));
    assert!(text.contains("  --name arg"));
    assert!(text.contains("\nNested:\n"));
    assert!(text.contains("--section.key arg"));
    // A name longer than the column is put on a line of its own.
    let mut wide = OptionsDescription::new("");
    wide.add_options().opt(
        "a-really-long-option-name-that-is-longer-than-forty-columns",
        bpo::value::<u32>(),
        "described below",
    );
    wide.add_option(OptionDescription::new("short", None, "short one"));
    let text = wide.to_string();
    assert!(text.contains("columns arg\n"), "{text}");
    let desc = OptionDescription::new("x,y", Some(Box::new(bpo::value::<u8>())), "d");
    assert_eq!(desc.long_name(), "x");
    assert!(desc.semantic().is_some());
}

#[test]
fn validation_errors_describe_themselves() {
    assert!(validators::check_first_occurrence(false).is_ok());
    assert_eq!(validators::check_first_occurrence(true), Err(ValidationError::MultipleOccurrences));
    assert_eq!(validators::get_single_string(&args(&["a"])), Ok("a"));
    assert_eq!(
        validators::get_single_string(&args(&["a", "b"])),
        Err(ValidationError::MultipleValuesNotAllowed)
    );
    assert_eq!(validators::get_single_string(&[]), Err(ValidationError::AtLeastOneValueRequired));
    assert_eq!(validators::get_single_string_with(&[], true), Ok(""));
    for (e, text) in [
        (ValidationError::MultipleOccurrences, "option cannot be specified more than once"),
        (ValidationError::MultipleValuesNotAllowed, "option only takes a single argument"),
        (ValidationError::AtLeastOneValueRequired, "option requires at least one argument"),
        (
            ValidationError::InvalidOptionValue("v".into()),
            "the argument ('v') for option is invalid",
        ),
    ] {
        assert_eq!(e.to_string(), text);
        assert_eq!(bpo::Error::from(e).what(), text);
    }
}

#[test]
fn string_maps_parse_print_and_look_up() {
    let mut m = StringMap::new();
    read_string_map("a=1:b=2 ignored", &mut m).unwrap();
    assert_eq!(m.at(&"a".into()).map(Sstring::as_str), Some("1"));
    assert_eq!(m.count(&"b".into()), 1);
    assert_eq!(get_or_default(&m, &"b".into(), &"x".into()).as_str(), "2");
    assert_eq!(get_or_default(&m, &"c".into(), &"x".into()).as_str(), "x");
    assert!(!m.emplace("a".into(), "9".into()));
    assert!(m.emplace("c".into(), "3".into()));
    let mut printed: Vec<String> = m.to_string().split(':').map(String::from).collect();
    printed.sort();
    assert_eq!(printed, vec!["a=1", "b=2", "c=3"]);
    assert!(read_string_map("broken", &mut m).is_err());
    m.clear();
    assert!(m.find(&"a".into()).is_none());

    let collected: StringMap = [(Sstring::from("k"), Sstring::from("v"))].into_iter().collect();
    assert_eq!(collected.iter().count(), 1);
    let mut out = None;
    validate_string_map(&mut out, &args(&["x=1", "y=2:x=3"])).unwrap();
    let out = out.unwrap();
    assert_eq!(out.at(&"x".into()).map(Sstring::as_str), Some("3"));

    // String maps are option values too.
    let mut d = OptionsDescription::new("");
    d.add_options().opt("map", bpo::value::<StringMap>(), "a map");
    let vm = parse(&d, &["--map", "p=q"]).unwrap();
    assert_eq!(vm.get::<StringMap>("map").unwrap().at(&"p".into()).map(Sstring::as_str), Some("q"));
}

#[test]
fn values_switches_and_groups() {
    let mut g = OptionGroup::new("group");
    assert_eq!(g.name(), "group");
    assert!(g.used());
    // A group without values is false.
    assert!(!g.as_bool());
    assert!(g.values().is_empty());
    assert!(g.values_mut().is_empty());
    let unused = OptionGroup::new_unused("unused", Unused);
    assert!(!unused.used());

    let mut v = Value::new("v", Some(5u32), "five");
    assert!(v.as_bool());
    assert!(v.defaulted());
    assert_eq!(v.name(), "v");
    assert_eq!(v.description(), "five");
    assert!(v.used());
    *v.get_value_mut() = 6;
    v.set_value(7);
    assert!(!v.defaulted());
    assert_eq!(*v.get_value(), 7);
    v.set_default_value(8);
    assert!(v.defaulted());
    let empty = Value::<u32>::new("e", None, "none");
    assert!(!empty.as_bool());
    let unused_value = Value::<u32>::new_unused("u", Unused);
    assert!(!unused_value.used());
    let missing = std::panic::catch_unwind(|| *Value::<u32>::new("m", None, "").get_value());
    assert!(missing.is_err());

    let mut s = Switch::new("s", "a switch");
    assert!(!s.as_bool());
    s.set_value();
    assert!(s.as_bool());
    s.unset_value();
    assert!(!s.as_bool());
    assert_eq!(s.name(), "s");
    assert!(!Switch::new_unused("su", Unused).used());
}

fn candidates() -> Vec<Candidate<u32>> {
    vec![
        Candidate { name: "one".into(), value: ValueHandle::from_box(Box::new(1)), opts: None },
        Candidate {
            name: "two".into(),
            value: ValueHandle::from_box(Box::new(2)),
            opts: Some(Box::pin(OptionGroup::new("two options"))),
        },
    ]
}

#[test]
fn selection_values_choose_among_candidates() {
    let mut sel = SelectionValue::new("pick", candidates(), "choose one");
    assert!(!sel.as_bool());
    assert!(sel.defaulted());
    assert_eq!(sel.get_candidate_names(), vec!["one".to_string(), "two".to_string()]);
    assert!(sel.select_candidate("one").unwrap().is_none());
    assert!(sel.as_bool());
    assert!(!sel.defaulted());
    assert_eq!(sel.get_selected_candidate_name(), "one");
    assert_eq!(*sel.get_selected_candidate(), 1);
    *sel.get_selected_candidate_mut() = 10;
    assert_eq!(*sel.get_selected_candidate(), 10);
    assert!(sel.get_selected_candidate_opts().is_none());

    let group = sel.select_default_candidate("two").unwrap().expect("two has options");
    assert_eq!(group.name(), "two options");
    assert!(sel.defaulted());
    assert_eq!(sel.get_selected_candidate_opts().unwrap().name(), "two options");
    assert!(sel.get_selected_candidate_opts_mut().is_some());
    assert!(sel.get_selected_candidate_opts_as::<OptionGroup>().is_some());
    assert!(sel.get_selected_candidate_opts_as_mut::<OptionGroup>().is_some());
    assert!(sel.get_selected_candidate_opts_as::<u32>().is_none());
    let e = sel.select_candidate("three").err().unwrap();
    assert!(e.to_string().contains("failed to find candidate three"), "{e}");

    let with_default = SelectionValue::new_with_default("p", candidates(), "two", "d").unwrap();
    assert_eq!(with_default.get_selected_candidate_name(), "two");
    assert!(SelectionValue::new_with_default("p", candidates(), "zero", "d").is_err());
    let unused = SelectionValue::<u32>::new_unused("p", Unused);
    assert!(!unused.used());
    assert!(!unused.as_bool());

    // A handle with its own deleter runs it once.
    let deleted = Rc::new(Cell::new(0));
    let d2 = deleted.clone();
    let raw = Box::into_raw(Box::new(5u8));
    // SAFETY: `raw` comes from a box, and the deleter frees it exactly once.
    let h = unsafe {
        ValueHandle::new(
            raw,
            Box::new(move |p| {
                d2.set(d2.get() + 1);
                drop(Box::from_raw(p));
            }),
        )
    };
    drop(h);
    assert_eq!(deleted.get(), 1);
}
