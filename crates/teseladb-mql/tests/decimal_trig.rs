//! Decimal trigonometric and hyperbolic functions (010 T007): correctly rounded to 34 digits in
//! the reference server's form, and their special values and domains (measured on a reference
//! router 9.0.4).

use teseladb_bson::Decimal128;
use teseladb_mql::decimal_trig::{Trig, atan2, trig};

fn d(s: &str) -> Decimal128 {
    s.parse().unwrap_or_else(|e| panic!("{s}: {e:?}"))
}

fn trig_of(name: &str) -> Trig {
    match name {
        "sin" => Trig::Sin,
        "cos" => Trig::Cos,
        "tan" => Trig::Tan,
        "asin" => Trig::Asin,
        "acos" => Trig::Acos,
        "atan" => Trig::Atan,
        "sinh" => Trig::Sinh,
        "cosh" => Trig::Cosh,
        "tanh" => Trig::Tanh,
        "asinh" => Trig::Asinh,
        "acosh" => Trig::Acosh,
        "atanh" => Trig::Atanh,
        "degreesToRadians" => Trig::DegreesToRadians,
        "radiansToDegrees" => Trig::RadiansToDegrees,
        _ => Trig::Sigmoid,
    }
}

/// Each value correctly rounded to 34 digits, as an independent computation (Python's `decimal`
/// at 160 digits, π to 6 800 digits for the reductions) gives it, in the reference server's form.
#[test]
fn values_are_correctly_rounded() {
    let cases: &[(&str, &str, &str, &str)] = &[
        ("sin", "1", "", "0.8414709848078965066525023216302990"),
        ("sin", "100", "", "-0.5063656411097587936565576104597854"),
        ("sin", "-3.5", "", "0.3507832276896198481203688000436356"),
        ("sin", "1E+6000", "", "-0.7249266534376325863453460198136025"),
        ("sin", "-1E+100", "", "0.3723761236612766882620866955531643"),
        ("cos", "1", "", "0.5403023058681397174009366074429766"),
        ("cos", "1E+61", "", "-0.9311323962746036597380985297854214"),
        ("cos", "1E-20", "", "1"),
        ("tan", "1", "", "1.557407724654902230506974807458360"),
        ("tan", "1E+20", "", "-0.8446024630198842541840932340005536"),
        ("asin", "0.5", "", "0.5235987755982988730771072305465838"),
        ("acos", "0.5", "", "1.047197551196597746154214461093168"),
        ("asin", "1", "", "1.570796326794896619231321691639751"),
        ("acos", "-1", "", "3.141592653589793238462643383279503"),
        ("atan", "1", "", "0.7853981633974483096156608458198757"),
        ("atan", "1E+40", "", "1.570796326794896619231321691639751"),
        ("atan", "-1E+70", "", "-1.570796326794896619231321691639751"),
        ("sinh", "1", "", "1.175201193643801456882381850595601"),
        ("sinh", "100", "", "1.344058570908067724206312775790007E+43"),
        ("sinh", "-100", "", "-1.344058570908067724206312775790007E+43"),
        ("cosh", "1", "", "1.543080634815243778477905620757062"),
        ("cosh", "-2", "", "3.762195691083631459562213477773746"),
        ("cosh", "60", "", "57100369490784214183147859.15723828"),
        ("tanh", "1", "", "0.7615941559557648881194582826047936"),
        ("tanh", "50", "", "1"),
        ("tanh", "-60", "", "-1"),
        ("asinh", "1", "", "0.8813735870195430252326093249797923"),
        ("asinh", "-1", "", "-0.8813735870195430252326093249797923"),
        ("asinh", "1E+70", "", "161.8741036901431431906766339493637"),
        ("acosh", "2", "", "1.316957896924816708625046347307968"),
        ("acosh", "1E+6144", "", "14147.77595853597662791595672970219"),
        ("atanh", "0.5", "", "0.5493061443340548456976226184612629"),
        ("degreesToRadians", "180", "", "3.141592653589793238462643383279503"),
        ("degreesToRadians", "1", "", "0.01745329251994329576923690768488613"),
        ("radiansToDegrees", "1", "", "57.29577951308232087679815481410517"),
        ("sigmoid", "1", "", "0.7310585786300048792511592418218363"),
        ("sigmoid", "-1", "", "0.2689414213699951207488407581781637"),
        ("sigmoid", "100", "", "1"),
        ("sigmoid", "1E-33", "", "0.5000000000000000000000000000000002"),
        ("sigmoid", "-1E-33", "", "0.4999999999999999999999999999999998"),
        ("sigmoid", "-100", "", "3.720075976020835962959695803863118E-44"),
        ("atan2", "1", "2", "0.4636476090008061162142562314612144"),
        ("atan2", "-1", "-2", "-2.677945044588987122248387151818288"),
        ("atan2", "1", "-1", "2.356194490192344928846982537459627"),
        ("atan2", "1", "1E+37", "1E-37"),
        ("atan2", "1E+70", "1", "1.570796326794896619231321691639751"),
        ("atan2", "-1", "3E+37", "-3.333333333333333333333333333333333E-38"),
    ];
    let wrong: Vec<String> = cases
        .iter()
        .filter_map(|(f, x, y, want)| {
            let got = if *f == "atan2" { atan2(d(x), d(y)) } else { trig(trig_of(f), d(x)) };
            let got = got.map(|v| v.to_string()).unwrap_or_default();
            (got != *want).then(|| format!("{f}({x}{y}) = {got}, not {want}"))
        })
        .collect();
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}

#[test]
fn special_values_and_domains() {
    let nan = Decimal128::nan();
    let inf = Decimal128::infinity(false);
    let ninf = Decimal128::infinity(true);
    assert_eq!(trig(Trig::Sin, nan).map(|v| v.to_string()), Some("NaN".into()));
    for f in [Trig::Sin, Trig::Cos, Trig::Tan, Trig::Asin, Trig::Acos, Trig::Atanh] {
        assert_eq!(trig(f, inf), None, "{f:?}");
    }
    assert_eq!(trig(Trig::Acosh, ninf), None);
    assert_eq!(trig(Trig::Acosh, inf).map(|v| v.to_string()), Some("Infinity".into()));
    assert_eq!(trig(Trig::Atan, ninf).map(|v| v.to_string().starts_with("-1.5707963")), Some(true));
    assert_eq!(trig(Trig::Tanh, ninf).map(|v| v.to_string()), Some("-1".into()));
    assert_eq!(trig(Trig::Cosh, ninf).map(|v| v.to_string()), Some("Infinity".into()));
    assert_eq!(trig(Trig::Sigmoid, ninf).map(|v| v.to_string()), Some("0E-6176".into()));
    assert_eq!(trig(Trig::Sinh, ninf).map(|v| v.to_string()), Some("-Infinity".into()));
    assert_eq!(trig(Trig::Sin, d("0E+5")).map(|v| v.to_string()), Some("0E+5".into()));
    assert_eq!(trig(Trig::Asin, d("2")), None);
    assert_eq!(trig(Trig::Acos, d("-1.5")), None);
    assert_eq!(trig(Trig::Atanh, d("2")), None);
    assert_eq!(trig(Trig::Atanh, d("1")).map(|v| v.to_string()), Some("Infinity".into()));
    assert_eq!(trig(Trig::Atanh, d("-1")).map(|v| v.to_string()), Some("-Infinity".into()));
    assert_eq!(trig(Trig::Acosh, d("0.5")), None);
    assert_eq!(trig(Trig::Acosh, d("-2")), None);
    assert_eq!(trig(Trig::Asin, d("-1")).map(|v| v.to_string().starts_with("-1.57")), Some(true));
    // Past 10^60 sin, cos and tan reduce exactly; the others take their limits.
    assert_eq!(
        trig(Trig::Sin, d("1E+70")).map(|v| v.to_string()),
        Some("-0.1480991313715503888389426819214873".into())
    );
    assert_eq!(trig(Trig::Asin, d("1E+70")), None);
    assert_eq!(trig(Trig::Acosh, d("-1E+70")), None);
    assert_eq!(trig(Trig::Sinh, d("-1E+70")).map(|v| v.to_string()), Some("-Infinity".into()));
    assert_eq!(trig(Trig::Cosh, d("1E+70")).map(|v| v.to_string()), Some("Infinity".into()));
    assert_eq!(trig(Trig::Tanh, d("-1E+70")).map(|v| v.to_string()), Some("-1".into()));
    assert_eq!(trig(Trig::Sigmoid, d("-1E+70")).map(|v| v.to_string()), Some("0E-6176".into()));
    assert_eq!(trig(Trig::Sigmoid, d("1E+70")).map(|v| v.to_string()), Some("1".into()));
    // Below 10^−40.
    assert_eq!(
        trig(Trig::Sin, d("-1E-45")).map(|v| v.to_string()),
        Some("-1.000000000000000000000000000000000E-45".into())
    );
    assert_eq!(trig(Trig::Cos, d("1E-45")).map(|v| v.to_string()), Some("1".into()));
    assert_eq!(
        trig(Trig::Cosh, d("1E-40")).map(|v| v.to_string()),
        Some("1.000000000000000000000000000000000".into())
    );
    assert_eq!(trig(Trig::Cosh, d("5E-40")).map(|v| v.to_string()), Some("1".into()));
    assert_eq!(trig(Trig::Sigmoid, d("1E-45")).map(|v| v.to_string()), Some("0.5".into()));
    assert_eq!(trig(Trig::Acosh, d("1E-45")), None);
    assert!(trig(Trig::Acos, d("1E-45")).expect("a value").to_string().starts_with("1.5707963"));
    // Zeros keep the reference server's exponents.
    for (f, want) in [
        (Trig::Tan, "0E-37"),
        (Trig::Asin, "0E-37"),
        (Trig::Sinh, "0E-37"),
        (Trig::Asinh, "0E-6176"),
        (Trig::Atanh, "0E-6176"),
        (Trig::DegreesToRadians, "0E-32"),
        (Trig::RadiansToDegrees, "0E-29"),
        (Trig::Cos, "1.000000000000000000000000000000000"),
        (Trig::Sigmoid, "0.5"),
        (Trig::Tanh, "0E+3"),
    ] {
        assert_eq!(trig(f, d("0E+3")).map(|v| v.to_string()), Some(want.into()), "{f:?}");
    }
    assert_eq!(trig(Trig::Acosh, d("0")), None);
    assert!(trig(Trig::Acos, d("0")).expect("a value").to_string().starts_with("1.5707963"));
    // atan2 with infinities.
    let inf_s = |y: &str, x: &str| atan2(d(y), d(x)).map(|v| v.to_string()).unwrap_or_default();
    assert_eq!(inf_s("Infinity", "Infinity"), "0.7853981633974483096156608458198757");
    assert_eq!(inf_s("-Infinity", "-Infinity"), "-2.356194490192344928846982537459627");
    assert_eq!(inf_s("-1", "Infinity"), "-0E-6176");
    assert_eq!(inf_s("1", "-Infinity"), "3.141592653589793238462643383279503");
    assert_eq!(inf_s("Infinity", "1"), "1.570796326794896619231321691639751");
    assert_eq!(inf_s("NaN", "1"), "NaN");
    assert_eq!(inf_s("0", "-5"), "3.141592653589793238462643383279503");
    assert_eq!(atan2(nan, d("1")).map(|v| v.to_string()), Some("NaN".into()));
    assert_eq!(atan2(d("0"), d("0")).map(|v| v.to_string()), Some("0E-6176".into()));
    assert!(atan2(d("-1"), d("0")).expect("a value").to_string().starts_with("-1.57"));
    assert!(atan2(d("1"), d("-1")).expect("a value").to_string().starts_with("2.35"));
    // Hyperbolic functions of large arguments.
    assert!(trig(Trig::Sinh, d("-100")).expect("a value").to_string().starts_with("-1.344"));
    assert_eq!(trig(Trig::Tanh, d("-60")).map(|v| v.to_string()), Some("-1".into()));
    let cosh = trig(Trig::Cosh, d("60")).expect("a value").to_string();
    assert!(cosh.starts_with("57100369490784214183147859.1572"), "{cosh}");
}
