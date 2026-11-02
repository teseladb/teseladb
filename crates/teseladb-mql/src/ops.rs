//! Operator and keyword names of MQL, each declared once.

/// `$eq`.
pub const EQ: &str = "$eq";
/// `$ne`.
pub const NE: &str = "$ne";
/// `$gt`.
pub const GT: &str = "$gt";
/// `$gte`.
pub const GTE: &str = "$gte";
/// The prefix of `$_internalExprEq` and the other comparisons an `$expr` implies.
pub const EXPR_PREFIX: &str = "$_internalExpr";
/// `$_internalExprGt`, of the partial filters of time series bucket indexes (015).
pub const EXPR_GT: &str = "$_internalExprGt";
/// `$_internalExprEq`, which RefDb adds beside an `$expr` equality of a path and a constant.
pub const EXPR_EQ: &str = "$_internalExprEq";
/// `$_internalExprGte`.
pub const EXPR_GTE: &str = "$_internalExprGte";
/// `$_internalExprLt`.
pub const EXPR_LT: &str = "$_internalExprLt";
/// `$_internalExprLte`.
pub const EXPR_LTE: &str = "$_internalExprLte";
/// `$lt`.
pub const LT: &str = "$lt";
/// `$lte`.
pub const LTE: &str = "$lte";
/// `$in`.
pub const IN: &str = "$in";
/// `$nin`.
pub const NIN: &str = "$nin";
/// `$and`.
pub const AND: &str = "$and";
/// `$or`.
pub const OR: &str = "$or";
/// `$nor`.
pub const NOR: &str = "$nor";
/// `$not`.
pub const NOT: &str = "$not";
/// `$exists`.
pub const EXISTS: &str = "$exists";
/// `$type`.
pub const TYPE: &str = "$type";
/// `$expr`.
pub const EXPR: &str = "$expr";
/// `$jsonSchema`.
pub const JSON_SCHEMA: &str = "$jsonSchema";
/// `$mod`.
pub const MOD: &str = "$mod";
/// `$regex`.
pub const REGEX: &str = "$regex";
/// `$options`.
pub const OPTIONS: &str = "$options";
/// `$text`.
pub const TEXT: &str = "$text";
/// `$where`.
pub const WHERE: &str = "$where";
/// `$all`.
pub const ALL: &str = "$all";
/// `$elemMatch`.
pub const ELEM_MATCH: &str = "$elemMatch";
/// `$size`.
pub const SIZE: &str = "$size";
/// `$bitsAllClear`.
pub const BITS_ALL_CLEAR: &str = "$bitsAllClear";
/// `$bitsAllSet`.
pub const BITS_ALL_SET: &str = "$bitsAllSet";
/// `$bitsAnyClear`.
pub const BITS_ANY_CLEAR: &str = "$bitsAnyClear";
/// `$bitsAnySet`.
pub const BITS_ANY_SET: &str = "$bitsAnySet";
/// `$geoWithin`.
pub const GEO_WITHIN: &str = "$geoWithin";
/// `$within`, the old name of `$geoWithin`.
pub const WITHIN: &str = "$within";
/// `$geoIntersects`.
pub const GEO_INTERSECTS: &str = "$geoIntersects";
/// `$near`.
pub const NEAR: &str = "$near";
/// `$nearSphere`.
pub const NEAR_SPHERE: &str = "$nearSphere";
/// `$maxDistance`.
pub const MAX_DISTANCE: &str = "$maxDistance";
/// `$minDistance`.
pub const MIN_DISTANCE: &str = "$minDistance";
/// `$geometry`.
pub const GEOMETRY: &str = "$geometry";
/// `$box`.
pub const BOX: &str = "$box";
/// `$polygon`.
pub const POLYGON: &str = "$polygon";
/// `$center`.
pub const CENTER: &str = "$center";
/// `$centerSphere`.
pub const CENTER_SPHERE: &str = "$centerSphere";
/// `$sampleRate`.
pub const SAMPLE_RATE: &str = "$sampleRate";
/// `$comment`.
pub const COMMENT: &str = "$comment";
/// `$alwaysTrue`.
pub const ALWAYS_TRUE: &str = "$alwaysTrue";
/// `$alwaysFalse`.
pub const ALWAYS_FALSE: &str = "$alwaysFalse";
/// `$search` of `$text`.
pub const SEARCH: &str = "$search";
/// `$language` of `$text`.
pub const LANGUAGE: &str = "$language";
/// `$caseSensitive` of `$text`.
pub const CASE_SENSITIVE: &str = "$caseSensitive";
/// `$diacriticSensitive` of `$text`.
pub const DIACRITIC_SENSITIVE: &str = "$diacriticSensitive";

/// `$slice`.
pub const SLICE: &str = "$slice";
/// `$meta`.
pub const META: &str = "$meta";

/// `$set`.
pub const SET: &str = "$set";
/// `$unset`.
pub const UNSET: &str = "$unset";
/// `$setOnInsert`.
pub const SET_ON_INSERT: &str = "$setOnInsert";
/// `$inc`.
pub const INC: &str = "$inc";
/// `$mul`.
pub const MUL: &str = "$mul";
/// `$min`.
pub const MIN: &str = "$min";
/// `$max`.
pub const MAX: &str = "$max";
/// `$rename`.
pub const RENAME: &str = "$rename";
/// `$currentDate`.
pub const CURRENT_DATE: &str = "$currentDate";
/// `$addToSet`.
pub const ADD_TO_SET: &str = "$addToSet";
/// `$pop`.
pub const POP: &str = "$pop";
/// `$pull`.
pub const PULL: &str = "$pull";
/// `$pullAll`.
pub const PULL_ALL: &str = "$pullAll";
/// `$push`.
pub const PUSH: &str = "$push";
/// `$bit`.
pub const BIT: &str = "$bit";
/// `$each`.
pub const EACH: &str = "$each";
/// `$position`.
pub const POSITION: &str = "$position";
/// `$sort`.
pub const SORT: &str = "$sort";

/// `_id`.
pub const ID: &str = "_id";
