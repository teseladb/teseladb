---------------------------- MODULE SplitView ----------------------------
(* The view of a range while it splits (005 FR-028, FR-031; 007 FR-007). Range G holds the keys   *)
(* "a" and "b" and splits at "b", which goes to the new range G2. From the moment the split       *)
(* starts no write of G takes "b"; once those in flight are applied the cut goes into G's log,    *)
(* and when G applies it G2 is born and, later, answers writes of "b" itself. The core publishes  *)
(* a view of G now and then: the keys G holds and the mark of the last write it applied. A scan   *)
(* of a key takes the view published for it and reads it later at the view's mark.                *)
(* G used to publish no view from the start of the split to its end, and every scan of the range  *)
(* went to its core meanwhile. With ViewsWhileSplitting it keeps publishing: the turn that        *)
(* applies the cut publishes the view of the keys G keeps (NarrowsAtCut) before G2 can answer     *)
(* anything, so a scan that begins after a write of G2 was answered finds no view of G for "b".   *)
(* NarrowsAtCut = FALSE leaves the view of both keys published after the cut: a scan of "b" then  *)
(* reads G's view below a write that G2 already answered, and ScansSeeAnswered fails.             *)
(* Invariant: a scan served holds every write of its key answered before it began.                *)
EXTENDS Naturals, FiniteSets

CONSTANTS MaxWrites, MaxScans, ViewsWhileSplitting, NarrowsAtCut

VARIABLES
    mark,       \* the mark of the last write: each write takes the next one
    applied,    \* G's writes applied: <<key, mark>>
    answered,   \* writes answered to their client: <<key, mark>>, of G or of G2
    split,      \* "no", "started" (no write of G takes "b"), "cut" (G applied the cut)
    g2,         \* whether G2 answers writes
    view,       \* the view of G published: whether there is one, its keys and its mark
    scan,       \* a scan between its steps
    served      \* scans served: <<key, mark read at, writes of the key answered when it began>>

vars == <<mark, applied, answered, split, g2, view, scan, served>>

Keys == {"a", "b"}
NoScan == [on |-> FALSE, key |-> "a", at |-> 0, must |-> {}]
Max(s) == IF s = {} THEN 0 ELSE CHOOSE m \in s : \A n \in s : n <= m
Last == Max({w[2] : w \in applied})
Held == IF split = "cut" THEN {"a"} ELSE Keys

Init ==
    /\ mark = 0
    /\ applied = {}
    /\ answered = {}
    /\ split = "no"
    /\ g2 = FALSE
    /\ view = [on |-> FALSE, keys |-> Keys, at |-> 0]
    /\ scan = NoScan
    /\ served = {}

\* G applies a write of a key it holds; once its split started, not of "b".
WriteG ==
    /\ mark < MaxWrites
    /\ \E k \in Held :
        /\ split = "no" \/ k = "a"
        /\ applied' = applied \cup {<<k, mark + 1>>}
    /\ mark' = mark + 1
    /\ UNCHANGED <<answered, split, g2, view, scan, served>>

\* The core publishes the view of what G holds and has applied; while it splits, only with
\* ViewsWhileSplitting.
Publish ==
    /\ split /= "started" \/ ViewsWhileSplitting
    /\ view' = [on |-> TRUE, keys |-> Held, at |-> Last]
    /\ UNCHANGED <<mark, applied, answered, split, g2, scan, served>>

\* A write of G is answered after the core published a view that holds it, or none.
AnswerG ==
    /\ \E w \in applied \ answered :
        /\ view.on => (view.at >= w[2] \/ w[1] \notin view.keys)
        /\ answered' = answered \cup {w}
    /\ UNCHANGED <<mark, applied, split, g2, view, scan, served>>

\* The split starts: writes of "b" wait. Without ViewsWhileSplitting the view is withdrawn.
Start ==
    /\ split = "no"
    /\ split' = "started"
    /\ view' = IF ViewsWhileSplitting THEN view ELSE [view EXCEPT !.on = FALSE]
    /\ UNCHANGED <<mark, applied, answered, g2, scan, served>>

\* G applies the cut, and the same turn publishes the view of the keys it keeps.
Cut ==
    /\ split = "started"
    /\ split' = "cut"
    /\ view' = IF NarrowsAtCut THEN [on |-> TRUE, keys |-> {"a"}, at |-> Last] ELSE view
    /\ UNCHANGED <<mark, applied, answered, g2, scan, served>>

\* G2 has a leader that applied what it was born with.
Born ==
    /\ split = "cut" /\ ~g2
    /\ g2' = TRUE
    /\ UNCHANGED <<mark, applied, answered, split, view, scan, served>>

\* G2 applies and answers a write of "b".
WriteG2 ==
    /\ g2 /\ mark < MaxWrites
    /\ mark' = mark + 1
    /\ answered' = answered \cup {<<"b", mark + 1>>}
    /\ UNCHANGED <<applied, split, g2, view, scan, served>>

\* A scan of a key takes the view of G published for it; it has to see the writes of the key
\* answered so far.
ScanBegin ==
    /\ ~scan.on
    /\ view.on
    /\ Cardinality(served) < MaxScans
    /\ \E k \in view.keys :
        scan' = [on |-> TRUE, key |-> k, at |-> view.at,
                 must |-> {w[2] : w \in {x \in answered : x[1] = k}}]
    /\ UNCHANGED <<mark, applied, answered, split, g2, view, served>>

\* ...and reads it at the view's mark.
ScanRead ==
    /\ scan.on
    /\ served' = served \cup {<<scan.key, scan.at, scan.must>>}
    /\ scan' = NoScan
    /\ UNCHANGED <<mark, applied, answered, split, g2, view>>

Next == WriteG \/ Publish \/ AnswerG \/ Start \/ Cut \/ Born \/ WriteG2 \/ ScanBegin \/ ScanRead

Spec == Init /\ [][Next]_vars

ScansSeeAnswered == \A s \in served : \A m \in s[3] : m <= s[2]

TypeOK ==
    /\ mark \in 0..MaxWrites
    /\ split \in {"no", "started", "cut"}
    /\ g2 \in BOOLEAN
    /\ view.on \in BOOLEAN
    /\ view.keys \subseteq Keys
    /\ scan.on \in BOOLEAN
=============================================================================
