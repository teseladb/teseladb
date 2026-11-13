--------------------------- MODULE ViewWithdrawn ---------------------------
(* The view of a range once another leader of its keys starts on the same node (005 FR-028,       *)
(* FR-005). A core publishes a view of each range it leads; a thread outside the core takes it,   *)
(* takes a mark from the node's clock and reads the published version of the LSM at that mark.    *)
(* The keys of the view pass to a successor on the same node: the half a split cuts from the      *)
(* range, the same replica in a later term, or the replica that another core imports. With one    *)
(* voter the successor leads at once, while the view is still within the span it was published    *)
(* for, and it takes the marks of its writes from the same clock.                                 *)
(* The core withdraws the view before the successor starts, and the thread looks at that after    *)
(* it takes its mark (Order = "mark-check"): a view it finds standing was standing when the mark  *)
(* was taken, so every write of the successor takes a later mark. A thread that does not look     *)
(* (Order = "none"), or that looks before it takes its mark (Order = "check-mark"), reads above a *)
(* write of the successor that is not applied yet, and ReadsAreStable fails.                      *)
(* The writes of the leader that published the view are in HlcLatches.tla and HlcKeyLatches.tla;  *)
(* here it has none in flight for these keys.                                                     *)
EXTENDS Naturals, FiniteSets

CONSTANTS MaxWrites, Order

VARIABLES
    clock,      \* the node's clock: each mark is the next one
    withdrawn,  \* the core withdrew the view
    successor,  \* the successor leads the keys
    inflight,   \* marks of the successor's writes not applied yet
    applied,    \* marks of its writes applied
    pc,         \* the thread: "idle", "loaded", "marked", "checked", "ready", "core", "done"
    mark,       \* the mark the thread read at
    saw         \* the writes it saw

vars == <<clock, withdrawn, successor, inflight, applied, pc, mark, saw>>

Init ==
    /\ clock = 0
    /\ withdrawn = FALSE
    /\ successor = FALSE
    /\ inflight = {}
    /\ applied = {}
    /\ pc = "idle"
    /\ mark = 0
    /\ saw = {}

\* The thread takes the view while it is published.
Load ==
    /\ pc = "idle" /\ ~withdrawn
    /\ pc' = "loaded"
    /\ UNCHANGED <<clock, withdrawn, successor, inflight, applied, mark, saw>>

\* It takes its mark: first, or after it looked at the view when it looks first.
Mark ==
    /\ \/ pc = "loaded" /\ Order /= "check-mark"
       \/ pc = "checked"
    /\ clock' = clock + 1
    /\ mark' = clock + 1
    /\ pc' = IF Order = "mark-check" THEN "marked" ELSE "ready"
    /\ UNCHANGED <<withdrawn, successor, inflight, applied, saw>>

\* It looks at whether the view was withdrawn: the read of a withdrawn view goes to the core.
Check ==
    /\ \/ pc = "marked" /\ Order = "mark-check"
       \/ pc = "loaded" /\ Order = "check-mark"
    /\ pc' = IF withdrawn THEN "core"
             ELSE IF Order = "mark-check" THEN "ready" ELSE "checked"
    /\ UNCHANGED <<clock, withdrawn, successor, inflight, applied, mark, saw>>

\* It reads what is applied at or below its mark.
Read ==
    /\ pc = "ready"
    /\ saw' = {w \in applied : w <= mark}
    /\ pc' = "done"
    /\ UNCHANGED <<clock, withdrawn, successor, inflight, applied, mark>>

\* The core withdraws the view, and only then does the successor start.
Withdraw ==
    /\ ~withdrawn
    /\ withdrawn' = TRUE
    /\ UNCHANGED <<clock, successor, inflight, applied, pc, mark, saw>>

Start ==
    /\ withdrawn /\ ~successor
    /\ successor' = TRUE
    /\ UNCHANGED <<clock, withdrawn, inflight, applied, pc, mark, saw>>

\* A write of the successor takes its mark from the node's clock and is applied later.
Write ==
    /\ successor
    /\ Cardinality(inflight \cup applied) < MaxWrites
    /\ clock' = clock + 1
    /\ inflight' = inflight \cup {clock + 1}
    /\ UNCHANGED <<withdrawn, successor, applied, pc, mark, saw>>

Apply ==
    /\ \E w \in inflight :
        /\ inflight' = inflight \ {w}
        /\ applied' = applied \cup {w}
    /\ UNCHANGED <<clock, withdrawn, successor, pc, mark, saw>>

Next == Load \/ Mark \/ Check \/ Read \/ Withdraw \/ Start \/ Write \/ Apply

Spec == Init /\ [][Next]_vars

Marks == 0..(MaxWrites + 1)

TypeOK ==
    /\ clock \in Marks
    /\ withdrawn \in BOOLEAN /\ successor \in BOOLEAN
    /\ inflight \subseteq Marks /\ applied \subseteq Marks
    /\ pc \in {"idle", "loaded", "marked", "checked", "ready", "core", "done"}
    /\ mark \in Marks
    /\ saw \subseteq applied

\* A read served holds every write at or below its mark, applied before or after it.
ReadsAreStable ==
    pc = "done" => \A w \in inflight \cup applied : w <= mark => w \in saw
=============================================================================
