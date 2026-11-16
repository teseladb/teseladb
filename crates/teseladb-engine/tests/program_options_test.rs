//! Tests of the command-line option parser: string-map options given several times.

#![cfg(target_os = "linux")]

use teseladb::engine::core::program_options::bpo;
use teseladb::engine::core::sstring::Sstring;
use teseladb::engine::util::program_options::StringMap;

fn parse(desc: &bpo::OptionsDescription, args: &[&str]) -> Result<bpo::VariablesMap, bpo::Error> {
    // Parse the arguments as a command line after the program name (the parser takes
    // the arguments without it).
    let raw_args: Vec<String> = args.iter().map(|arg| arg.to_string()).collect();

    let mut vars = bpo::VariablesMap::default();
    bpo::store(bpo::command_line_parser(raw_args).options(desc).run()?, &mut vars)?;
    bpo::notify(&mut vars)?;

    Ok(vars)
}

#[test]
fn string_map() {
    let mut desc = bpo::OptionsDescription::new("");
    desc.add_options().opt("ages", bpo::value::<StringMap>(), "");

    let vars =
        parse(&desc, &["--ages", "joe=15:sally=20", "--ages", "phil=18:joe=11"]).expect("parse");
    let ages = vars.get::<StringMap>("ages").expect("ages");

    // `string_map` values can be specified multiple times. The last association takes precedence.
    assert_eq!(ages.at(&Sstring::from("joe")).map(Sstring::as_str), Some("11"));
    assert_eq!(ages.at(&Sstring::from("phil")).map(Sstring::as_str), Some("18"));
    assert_eq!(ages.at(&Sstring::from("sally")).map(Sstring::as_str), Some("20"));

    // An invalid option value.
    let err = match parse(&desc, &["--ages", "tim:"]) {
        Ok(_) => panic!("expected bpo::invalid_option_value"),
        Err(e) => e,
    };
    assert!(err.what().contains("is invalid"));
}
