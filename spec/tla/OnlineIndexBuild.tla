---------------------------- MODULE OnlineIndexBuild ----------------------------
(* Building a secondary index while the collection takes writes (009 FR-006). Group 0 moves the  *)
(* index through "deleteOnly" and "writeOnly"; each move waits until the leader of every range   *)
(* of the collection has applied the state before it (the barrier). A leader applies Group 0 at  *)
(* its own pace and writes with the state it has: without the index it touches no entry, in     *)
(* "deleteOnly" it only deletes the entries of the version it replaces, from "writeOnly" on it  *)
(* deletes and writes them. Once every leader is in "writeOnly", the backfill takes mark T and,  *)
(* document by document, writes the entry of the version at T unless the document has a newer   *)
(* version (whose write kept the index already). When every document is done the index is       *)
(* "ready".                                                                                      *)
(* Invariant: a ready index holds exactly the entries of the documents' current versions.        *)
(* With Barrier = FALSE the states move on without waiting for the leaders: a leader that lags   *)
(* writes a document after T without its entry, and TLC reports ReadyIsExact violated (the       *)
(* faulty variant «a lagging leader that does not keep the index»).                              *)
EXTENDS Naturals, FiniteSets, Sequences

CONSTANTS Range, Val, MaxTs, Barrier

\* One document in each range, named after it: enough for a leader to lag behind the others.
Doc == Range
RangeOf == [d \in Doc |-> d]

None == 0
States == <<"absent", "deleteOnly", "writeOnly", "backfill", "ready">>
Rank(s) == CHOOSE i \in 1..5 : States[i] = s

VARIABLES
    state,      \* Group 0's state of the index
    view,       \* the state each range's leader has applied
    now,        \* the newest mark given
    hist,       \* each document's versions: a set of <<mark, value>> (None: deleted)
    idx,        \* the index's entries: <<document, value>>
    mark,       \* T, the backfill's mark (0 before it starts)
    done        \* documents the backfill went through

vars == <<state, view, now, hist, idx, mark, done>>

\* The value of document d at mark t (None if it had no version then).
ValueAt(d, t) ==
    LET older == {v \in hist[d] : v[1] <= t}
    IN IF older = {} THEN None
       ELSE (CHOOSE v \in older : \A w \in older : w[1] <= v[1])[2]

Current(d) == ValueAt(d, now)
Newest(d) == IF hist[d] = {} THEN 0 ELSE (CHOOSE v \in hist[d] : \A w \in hist[d] : w[1] <= v[1])[1]

Init ==
    /\ state = "absent"
    /\ view = [r \in Range |-> "absent"]
    /\ now = 0
    /\ hist = [d \in Doc |-> {}]
    /\ idx = {}
    /\ mark = 0
    /\ done = {}

\* The leaders have all applied state s (or the barrier is left out).
Applied(s) == ~Barrier \/ \A r \in Range : Rank(view[r]) >= Rank(s)

Create ==
    /\ state = "absent"
    /\ state' = "deleteOnly"
    /\ UNCHANGED <<view, now, hist, idx, mark, done>>

ToWriteOnly ==
    /\ state = "deleteOnly"
    /\ Applied("deleteOnly")
    /\ state' = "writeOnly"
    /\ UNCHANGED <<view, now, hist, idx, mark, done>>

StartBackfill ==
    /\ state = "writeOnly"
    /\ Applied("writeOnly")
    /\ state' = "backfill"
    /\ mark' = now
    /\ UNCHANGED <<view, now, hist, idx, done>>

\* A leader applies the next state of Group 0.
Learn(r) ==
    /\ Rank(view[r]) < Rank(state)
    /\ view' = [view EXCEPT ![r] = States[Rank(view[r]) + 1]]
    /\ UNCHANGED <<state, now, hist, idx, mark, done>>

\* The leader of d's range writes value v (None: a delete) at a new mark.
Write(d, v) ==
    LET s == view[RangeOf[d]]
        old == Current(d)
        gone == IF old = None THEN {} ELSE {<<d, old>>}
        put == IF v = None THEN {} ELSE {<<d, v>>}
    IN /\ now < MaxTs
       /\ v /= old
       /\ now' = now + 1
       /\ hist' = [hist EXCEPT ![d] = @ \cup {<<now + 1, v>>}]
       /\ idx' = CASE s = "absent" -> idx
                   [] s = "deleteOnly" -> idx \ gone
                   [] OTHER -> (idx \ gone) \cup put
       /\ UNCHANGED <<state, view, mark, done>>

\* The backfill writes d's entry at T, unless d has a newer version.
Backfill(d) ==
    /\ state = "backfill"
    /\ d \notin done
    /\ LET v == ValueAt(d, mark)
       IN idx' = IF Newest(d) <= mark /\ v /= None THEN idx \cup {<<d, v>>} ELSE idx
    /\ done' = done \cup {d}
    /\ UNCHANGED <<state, view, now, hist, mark>>

Ready ==
    /\ state = "backfill"
    /\ done = Doc
    /\ state' = "ready"
    /\ UNCHANGED <<view, now, hist, idx, mark, done>>

Next ==
    \/ Create \/ ToWriteOnly \/ StartBackfill \/ Ready
    \/ \E r \in Range : Learn(r)
    \/ \E d \in Doc, v \in Val \cup {None} : Write(d, v)
    \/ \E d \in Doc : Backfill(d)

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ state \in {States[i] : i \in 1..5}
    /\ view \in [Range -> {States[i] : i \in 1..5}]
    /\ now \in 0..MaxTs
    /\ idx \subseteq Doc \X Val
    /\ done \subseteq Doc

\* A ready index holds exactly the entries of the current versions.
ReadyIsExact ==
    state = "ready" =>
        idx = {<<d, Current(d)>> : d \in {e \in Doc : Current(e) /= None}}

\* A leader never runs ahead of Group 0.
ViewsFollow == \A r \in Range : Rank(view[r]) <= Rank(state)
=============================================================================
