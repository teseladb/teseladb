------------------------------ MODULE SearchSegmentGc ------------------------------
(* The segments of a search index and the GC of the ranges they read (011 T049, FR-012).         *)
(* The segments are not a provider of the log's release: a node keeps them in memory and builds  *)
(* them from the versions of the ranges, scanning from the mark it applied up to (a full scan    *)
(* from mark 0 when it has none, after a restart for instance). What they depend on is the MVCC  *)
(* history: the GC of a range drops the deletions below its threshold, which is at or below the  *)
(* wall clock less the history retention (D-020). Segments whose mark fell below that floor     *)
(* would read the changes above it and miss a deletion the GC dropped; with RebuildBehind they  *)
(* start over from a full scan (`search_ops::behind_history`).                                  *)
(* Invariant: right after a catch-up the segments hold exactly the live documents.              *)
EXTENDS Integers, FiniteSets

CONSTANTS Keys, MaxTime, Retention, RebuildBehind

VARIABLES
    now,      \* cluster time, the timestamp of the last write
    hist,     \* the newest version of each key: its time, whether it is live, whether GC kept it
    floor,    \* the history floor: versions below it may be gone
    seg,      \* the documents the segments hold
    applied,  \* the mark the segments applied the versions up to (0: nothing)
    fresh     \* the segments just caught up

vars == <<now, hist, floor, seg, applied, fresh>>

Live == {k \in Keys : hist[k].live}

TypeOK ==
    /\ now \in 0..MaxTime
    /\ floor \in 0..MaxTime
    /\ applied \in 0..MaxTime
    /\ seg \subseteq Keys
    /\ fresh \in BOOLEAN
    /\ \A k \in Keys : hist[k].ts \in 0..MaxTime /\ hist[k].live \in BOOLEAN /\ hist[k].kept \in BOOLEAN

Init ==
    /\ now = 0
    /\ hist = [k \in Keys |-> [ts |-> 0, live |-> FALSE, kept |-> FALSE]]
    /\ floor = 0
    /\ seg = {}
    /\ applied = 0
    /\ fresh = TRUE

Write(k, live) ==
    /\ now < MaxTime
    /\ now' = now + 1
    /\ hist' = [hist EXCEPT ![k] = [ts |-> now + 1, live |-> live, kept |-> TRUE]]
    /\ fresh' = FALSE
    /\ UNCHANGED <<floor, seg, applied>>

\* The GC raises the floor to the clock less the retention and drops the deletions below it.
Gc ==
    /\ now - Retention > floor
    /\ floor' = now - Retention
    /\ hist' = [k \in Keys |->
                  IF ~hist[k].live /\ hist[k].ts < floor' THEN [hist[k] EXCEPT !.kept = FALSE]
                  ELSE hist[k]]
    /\ fresh' = FALSE
    /\ UNCHANGED <<now, seg, applied>>

\* What a scan from mark m reports: the kept versions above it.
Changed(m) == {k \in Keys : hist[k].kept /\ hist[k].ts > m}

CatchUp ==
    /\ IF RebuildBehind /\ applied # 0 /\ applied < floor
       THEN seg' = {k \in Changed(0) : hist[k].live}
       ELSE seg' = (seg \ Changed(applied)) \cup {k \in Changed(applied) : hist[k].live}
    /\ applied' = now
    /\ fresh' = TRUE
    /\ UNCHANGED <<now, hist, floor>>

Next ==
    \/ \E k \in Keys, live \in BOOLEAN : Write(k, live)
    \/ Gc
    \/ CatchUp

Spec == Init /\ [][Next]_vars

SegmentsExact == fresh => seg = Live
=====================================================================================
