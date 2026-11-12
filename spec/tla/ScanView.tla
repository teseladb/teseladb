---------------------------- MODULE ScanView ----------------------------
(* The view a scan outside the core reads (005 FR-031) and the writes of the range answered to     *)
(* their client, which a scan that begins afterwards has to see (DV-010).                          *)
(* One range in one term of its leader. Its writes are the entries 1, 2, ... of the log and are    *)
(* applied in that order. A write is answered before it is applied (`w: 1`, D-034) or once         *)
(* applied, and the range notes the highest entry it answered before the answer leaves. The core   *)
(* publishes a view now and then, with the last entry applied, or withdraws it (a split, a lease   *)
(* it cannot vouch for). A scan begins by taking the entry the range has noted; later it reads     *)
(* the view published then if that view applied the entry, waits for another view (005 FR-034) or  *)
(* goes to the core. Writes answered after it began do not hold it back: under a steady flow of    *)
(* `w: 1` writes every view lacks the newest of them.                                              *)
(* With EntryInView the scan reads only a view that applied the entry it took.                     *)
(* EntryInView = FALSE is the variant that counted the writes answered before they were applied    *)
(* and not applied yet, and read when there was none: a write answered and then applied after the  *)
(* view was published left none, and the scan read a view without it. That variant also needed     *)
(* the answer of an applied write to leave only after the core published what it applied           *)
(* (AnswersAfterViews); the entry in the view does not. LooksAtAnswered = FALSE drops the check.   *)
(* EntryInView = FALSE, with AnswersAfterViews TRUE or FALSE, and LooksAtAnswered = FALSE each     *)
(* violate ScansSeeAnswered.                                                                       *)
(* Invariant: a scan served holds every write answered before it began.                            *)
EXTENDS Naturals, FiniteSets

CONSTANTS MaxWrites, MaxScans, EntryInView, AnswersAfterViews, LooksAtAnswered

VARIABLES
    proposed,   \* entries in the leader's log: 1..proposed
    applied,    \* entries applied: 1..applied
    early,      \* entries answered before they were applied
    answered,   \* entries answered to their client
    noted,      \* the highest entry answered, as the range notes it
    published,  \* whether the core publishes a view of the range
    view,       \* the last entry applied in the view published
    scan,       \* a scan that began: the entry it took and the writes answered when it began
    served      \* scans served: <<last entry of the view read, writes answered when it began>>

vars == <<proposed, applied, early, answered, noted, published, view, scan, served>>

NoScan == [on |-> FALSE, need |-> 0, must |-> {}]

Init ==
    /\ proposed = 0
    /\ applied = 0
    /\ early = {}
    /\ answered = {}
    /\ noted = 0
    /\ published = FALSE
    /\ view = 0
    /\ scan = NoScan
    /\ served = {}

Propose ==
    /\ proposed < MaxWrites
    /\ proposed' = proposed + 1
    /\ UNCHANGED <<applied, early, answered, noted, published, view, scan, served>>

Apply ==
    /\ applied < proposed
    /\ applied' = applied + 1
    /\ UNCHANGED <<proposed, early, answered, noted, published, view, scan, served>>

\* The core publishes the view of what it has applied...
Publish ==
    /\ published' = TRUE
    /\ view' = applied
    /\ UNCHANGED <<proposed, applied, early, answered, noted, scan, served>>

\* ...or none.
Withdraw ==
    /\ published
    /\ published' = FALSE
    /\ UNCHANGED <<proposed, applied, early, answered, noted, view, scan, served>>

\* A write is answered before it is applied: the range notes its entry, then answers.
AnswerEarly ==
    /\ \E m \in (applied + 1)..proposed :
        /\ m \notin answered
        /\ early' = early \cup {m}
        /\ answered' = answered \cup {m}
        /\ noted' = IF m > noted THEN m ELSE noted
    /\ UNCHANGED <<proposed, applied, published, view, scan, served>>

\* A write is answered once applied; with AnswersAfterViews, after the core published a view
\* that holds it, or none.
AnswerApplied ==
    /\ \E m \in 1..applied :
        /\ m \notin answered
        /\ AnswersAfterViews => (published => view >= m)
        /\ answered' = answered \cup {m}
        /\ noted' = IF m > noted THEN m ELSE noted
    /\ UNCHANGED <<proposed, applied, early, published, view, scan, served>>

\* A scan begins: it takes the entry the range has noted, and has to see the writes answered so
\* far.
ScanBegin ==
    /\ ~scan.on
    /\ Cardinality(served) < MaxScans
    /\ scan' = [on |-> TRUE, need |-> noted, must |-> answered]
    /\ UNCHANGED <<proposed, applied, early, answered, noted, published, view, served>>

\* It reads the view published now if that view may serve it; otherwise it waits for another.
ScanRead ==
    /\ scan.on
    /\ published
    /\ \/ ~LooksAtAnswered
       \/ LooksAtAnswered /\ EntryInView /\ view >= scan.need
       \/ LooksAtAnswered /\ ~EntryInView /\ {m \in early : m > applied} = {}
    /\ served' = served \cup {<<view, scan.must>>}
    /\ scan' = NoScan
    /\ UNCHANGED <<proposed, applied, early, answered, noted, published, view>>

\* It goes to the core instead.
ScanGivesUp ==
    /\ scan.on
    /\ scan' = NoScan
    /\ UNCHANGED <<proposed, applied, early, answered, noted, published, view, served>>

Next == Propose \/ Apply \/ Publish \/ Withdraw \/ AnswerEarly \/ AnswerApplied
        \/ ScanBegin \/ ScanRead \/ ScanGivesUp

Spec == Init /\ [][Next]_vars

ScansSeeAnswered == \A s \in served : \A m \in s[2] : m <= s[1]

TypeOK ==
    /\ proposed \in 0..MaxWrites
    /\ applied \in 0..proposed
    /\ early \subseteq answered
    /\ answered \subseteq 1..proposed
    /\ noted \in 0..proposed
    /\ published \in BOOLEAN
    /\ view \in 0..applied
    /\ scan.on \in BOOLEAN
    /\ scan.need \in 0..noted
=============================================================================
