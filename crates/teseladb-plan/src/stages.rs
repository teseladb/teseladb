//! The aggregation stages of the reference server 9.0 (010 `contracts/stages.md`) and what a
//! pipeline checks of them before it runs: a stage is a document with exactly one field, the
//! stage's name, and some stages may not appear where the pipeline is going (a view definition, a
//! sub-pipeline).

/// How a stage reaches outside the pipeline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reach {
    /// It reads and transforms documents.
    Reads,
    /// It writes to a collection (`$out`, `$merge`).
    Writes,
    /// It opens a change stream (014).
    ChangeStream,
    /// The reference server keeps it for its own pipelines between nodes: a user's pipeline may not
    /// name it.
    Internal,
}

/// A stage the reference server knows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stage {
    /// Its name, with the `$`.
    pub name: &'static str,
    /// How it reaches outside the pipeline.
    pub reach: Reach,
}

/// Every stage, in byte order: the stage list of the 9.0 manual and the names the reference router
/// 9.0.4 recognizes (measured 31-10-2026: every other name is `Unrecognized pipeline stage name`).
pub const STAGES: &[Stage] = &[
    Stage { name: "$_internalInhibitOptimization", reach: Reach::Reads },
    Stage { name: "$_internalUnpackBucket", reach: Reach::Reads },
    Stage { name: "$addFields", reach: Reach::Reads },
    Stage { name: "$bucket", reach: Reach::Reads },
    Stage { name: "$bucketAuto", reach: Reach::Reads },
    Stage { name: "$changeStream", reach: Reach::ChangeStream },
    Stage { name: "$changeStreamSplitLargeEvent", reach: Reach::ChangeStream },
    Stage { name: "$collStats", reach: Reach::Reads },
    Stage { name: "$count", reach: Reach::Reads },
    Stage { name: "$currentOp", reach: Reach::Reads },
    Stage { name: "$densify", reach: Reach::Reads },
    Stage { name: "$documents", reach: Reach::Reads },
    Stage { name: "$facet", reach: Reach::Reads },
    Stage { name: "$fill", reach: Reach::Reads },
    Stage { name: "$geoNear", reach: Reach::Reads },
    Stage { name: "$graphLookup", reach: Reach::Reads },
    Stage { name: "$group", reach: Reach::Reads },
    Stage { name: "$indexStats", reach: Reach::Reads },
    Stage { name: "$limit", reach: Reach::Reads },
    Stage { name: "$listCachedAndActiveUsers", reach: Reach::Reads },
    Stage { name: "$listCatalog", reach: Reach::Reads },
    Stage { name: "$listClusterCatalog", reach: Reach::Reads },
    Stage { name: "$listExtensions", reach: Reach::Reads },
    Stage { name: "$listLocalSessions", reach: Reach::Reads },
    Stage { name: "$listMqlEntities", reach: Reach::Reads },
    Stage { name: "$listSampledQueries", reach: Reach::Reads },
    Stage { name: "$listSearchIndexes", reach: Reach::Reads },
    Stage { name: "$listSessions", reach: Reach::Reads },
    Stage { name: "$lookup", reach: Reach::Reads },
    Stage { name: "$match", reach: Reach::Reads },
    Stage { name: "$merge", reach: Reach::Writes },
    Stage { name: "$mergeCursors", reach: Reach::Internal },
    Stage { name: "$out", reach: Reach::Writes },
    Stage { name: "$planCacheStats", reach: Reach::Reads },
    Stage { name: "$project", reach: Reach::Reads },
    Stage { name: "$querySettings", reach: Reach::Reads },
    Stage { name: "$queryStats", reach: Reach::Reads },
    Stage { name: "$rankFusion", reach: Reach::Reads },
    Stage { name: "$redact", reach: Reach::Reads },
    Stage { name: "$replaceRoot", reach: Reach::Reads },
    Stage { name: "$replaceWith", reach: Reach::Reads },
    Stage { name: "$sample", reach: Reach::Reads },
    Stage { name: "$score", reach: Reach::Reads },
    Stage { name: "$scoreFusion", reach: Reach::Reads },
    Stage { name: "$search", reach: Reach::Reads },
    Stage { name: "$searchBeta", reach: Reach::Reads },
    Stage { name: "$searchMeta", reach: Reach::Reads },
    Stage { name: "$set", reach: Reach::Reads },
    Stage { name: "$setMetadata", reach: Reach::Internal },
    Stage { name: "$setVariableFromSubPipeline", reach: Reach::Internal },
    Stage { name: "$setWindowFields", reach: Reach::Reads },
    Stage { name: "$shardedDataDistribution", reach: Reach::Reads },
    Stage { name: "$skip", reach: Reach::Reads },
    Stage { name: "$sort", reach: Reach::Reads },
    Stage { name: "$sortByCount", reach: Reach::Reads },
    Stage { name: "$unionWith", reach: Reach::Reads },
    Stage { name: "$unset", reach: Reach::Reads },
    Stage { name: "$unwind", reach: Reach::Reads },
    Stage { name: "$vectorSearch", reach: Reach::Reads },
];

/// The stage named `name`.
pub fn stage(name: &str) -> Option<&'static Stage> {
    STAGES.binary_search_by(|s| s.name.cmp(name)).ok().map(|i| &STAGES[i])
}

/// Why a stage specification is refused, with the reference server's code and message (measured on
/// a the reference router 9.0.4).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StageError {
    /// The reference server's code.
    pub code: i32,
    /// The reference server's message.
    pub message: String,
}

/// A stage with more or fewer than one field.
pub const STAGE_ONE_FIELD: i32 = 40_323;
/// A stage name the reference server does not know.
pub const UNRECOGNIZED_STAGE: i32 = 40_324;
/// A stage the reference server keeps for its own pipelines.
pub const INTERNAL_STAGE: i32 = 5_491_300;

/// The stage of specification `fields` (the names of the document's fields).
pub fn named<'a>(fields: &[&'a str]) -> Result<(&'a str, &'static Stage), StageError> {
    let [name] = fields else {
        return Err(StageError {
            code: STAGE_ONE_FIELD,
            message: "A pipeline stage specification object must contain exactly one field."
                .to_owned(),
        });
    };
    match stage(name) {
        // The quote after the name is the reference server's (measured).
        Some(s) if s.reach == Reach::Internal => Err(StageError {
            code: INTERNAL_STAGE,
            message: format!("{name}' is not allowed in user requests"),
        }),
        Some(s) => Ok((name, s)),
        None => Err(StageError {
            code: UNRECOGNIZED_STAGE,
            message: format!("Unrecognized pipeline stage name: '{name}'"),
        }),
    }
}
