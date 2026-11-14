//! 004 FR-023, T057: the block compressor option and its key of the configuration file choose
//! the codec of the new blocks of every core; `snappy`, which the node has not, is
//! refused at start with the values it takes.

use teseladb_lsm::Codec;
use teseladb_server::cli::{BLOCK_COMPRESSOR, Command, parse, parse_with};

fn start(args: &[&str]) -> Result<Codec, String> {
    let argv: Vec<String> =
        ["start", "--insecure"].iter().chain(args).map(|s| (*s).to_owned()).collect();
    match parse(&argv) {
        Ok(Command::Start(o)) => Ok(teseladb_server::node_config(&o).lsm.codec),
        Ok(_) => panic!("start"),
        Err(e) => Err(e.0),
    }
}

#[test]
fn the_option_chooses_the_codec_of_the_new_blocks() {
    assert_eq!(BLOCK_COMPRESSOR, teseladb_names::OPTION_BLOCK_COMPRESSOR);
    let option = format!("--{BLOCK_COMPRESSOR}");
    assert_eq!(start(&[&option, "zstd"]), Ok(Codec::Zstd));
    assert_eq!(start(&[&option, "zlib"]), Ok(Codec::Zlib));
    assert_eq!(start(&[&format!("{option}=none")]), Ok(Codec::None));
    // 004 FR-037.
    assert_eq!(start(&[&option, "lz4"]), Ok(Codec::Lz4));
    // Without the option, zlib (D-164).
    assert_eq!(start(&[]), Ok(Codec::Zlib));
    assert_eq!(teseladb_server::DEFAULT_BLOCK_CODEC, Codec::Zlib);
    let snappy = start(&[&option, "snappy"]).unwrap_err();
    assert!(snappy.contains("none, zlib, zstd or lz4"), "{snappy}");
}

#[test]
fn the_configuration_file_names_it_as_the_reference_does() {
    let engine = teseladb_names::STORAGE_ENGINE;
    let yaml =
        format!("storage:\n  {engine}:\n    collectionConfig:\n      blockCompressor: zlib\n");
    let argv = ["start", "--insecure", "--config", "/c.yaml"].map(String::from);
    let Command::Start(o) = parse_with(&argv, &|_| Ok(yaml.as_bytes().to_vec())).unwrap() else {
        panic!("start")
    };
    assert_eq!(teseladb_server::node_config(&o).lsm.codec, Codec::Zlib);
}
