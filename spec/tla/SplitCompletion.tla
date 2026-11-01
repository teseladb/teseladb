---------------------------- MODULE SplitCompletion ----------------------------
(* When Group 0 completes a split of range L (007 FR-007, A007-3-1).                              *)
(* L's log holds its cuts and merges in order: Split(at, id) ends L at `at` and gives the rest to *)
(* the new range `id`; Merge gives L back its whole extent. Each replica applies a prefix of the *)
(* committed log, so a replica behind a merge still ends at the cut that merge undid. Group 0    *)
(* records a split (at, id), L's leader proposes it, and Group 0 completes it once a replica      *)
(* shows it applied. A replica shows it by its last cut, the one into `id` (`ByEnd` = FALSE).     *)
(* With `ByEnd` = TRUE the end decides, as the code did before A007-3-1: a second split at the   *)
(* key of a first one that a merge undid is completed on a replica behind that merge, and TLC    *)
(* finds `TableMatchesLog` broken (Group 0 records an end that the log does not give L).         *)
(* Invariant: with no split or merge in progress, Group 0's end of L is the end L's whole log    *)
(* gives, and every split Group 0 completed is in the log.                                       *)
EXTENDS Naturals, Sequences

CONSTANTS Node, Cut, MaxSplits, MaxLog, ByEnd

Full == "full"
NoSplit == [at |-> Full, id |-> 0]

VARIABLES
    log,        \* L's committed log: <<"split", at, id>> or <<"merge">>
    applied,    \* by node, the prefix of the log its replica of L applied
    table,      \* Group 0: L's end (Full: the whole space)
    pending,    \* Group 0: the split recorded and not completed (NoSplit: none)
    merging,    \* Group 0: a merge of L with its right neighbour in progress
    nextId,     \* the number Group 0 gives the next new range
    completed   \* the splits Group 0 completed

vars == <<log, applied, table, pending, merging, nextId, completed>>

\* L's end and last cut after the first `i` entries.
RECURSIVE EndAt(_)
EndAt(i) == IF i = 0 THEN Full
            ELSE IF log[i][1] = "split" THEN log[i][2] ELSE Full
RECURSIVE CutAt(_)
CutAt(i) == IF i = 0 THEN 0
            ELSE IF log[i][1] = "split" THEN log[i][3] ELSE CutAt(i - 1)

\* Whether node n's replica shows the pending split applied.
Shows(n) ==
    IF ByEnd THEN EndAt(applied[n]) = pending.at
    ELSE CutAt(applied[n]) = pending.id /\ EndAt(applied[n]) = pending.at

InLog(id) == \E i \in 1..Len(log) : log[i][1] = "split" /\ log[i][3] = id

Init ==
    /\ log = <<>> /\ applied = [n \in Node |-> 0] /\ table = Full /\ pending = NoSplit
    /\ merging = FALSE /\ nextId = 1 /\ completed = {}

\* Group 0 records a split of L at `at` (L whole, nothing else in progress).
SplitIntent(at) ==
    /\ pending = NoSplit /\ ~merging /\ table = Full /\ nextId <= MaxSplits
    /\ pending' = [at |-> at, id |-> nextId]
    /\ nextId' = nextId + 1
    /\ UNCHANGED <<log, applied, table, merging, completed>>

\* L's leader proposes the split once; it commits.
ProposeSplit ==
    /\ pending /= NoSplit /\ ~InLog(pending.id) /\ Len(log) < MaxLog
    /\ log' = Append(log, <<"split", pending.at, pending.id>>)
    /\ UNCHANGED <<applied, table, pending, merging, nextId, completed>>

Apply(n) ==
    /\ applied[n] < Len(log)
    /\ applied' = [applied EXCEPT ![n] = @ + 1]
    /\ UNCHANGED <<log, table, pending, merging, nextId, completed>>

Complete(n) ==
    /\ pending /= NoSplit /\ Shows(n)
    /\ table' = pending.at
    /\ completed' = completed \cup {pending.id}
    /\ pending' = NoSplit
    /\ UNCHANGED <<log, applied, merging, nextId>>

\* Group 0 records a merge of L with the range its last split made, and L applies it.
Merge ==
    /\ pending = NoSplit /\ ~merging /\ table /= Full /\ Len(log) < MaxLog
    /\ merging' = TRUE
    /\ log' = Append(log, <<"merge">>)
    /\ UNCHANGED <<applied, table, pending, nextId, completed>>

\* Group 0 completes the merge once a replica applied it.
MergeDone(n) ==
    /\ merging /\ applied[n] = Len(log) /\ log[Len(log)][1] = "merge"
    /\ merging' = FALSE /\ table' = Full
    /\ UNCHANGED <<log, applied, pending, nextId, completed>>

Next ==
    \/ \E at \in Cut : SplitIntent(at)
    \/ ProposeSplit \/ Merge
    \/ \E n \in Node : Apply(n) \/ Complete(n) \/ MergeDone(n)

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ applied \in [Node -> 0..MaxLog] /\ nextId \in 1..(MaxSplits + 1)
    /\ merging \in BOOLEAN /\ completed \subseteq 1..MaxSplits

TableMatchesLog ==
    /\ (pending = NoSplit /\ ~merging) => table = EndAt(Len(log))
    /\ \A id \in completed : InLog(id)
=============================================================================
