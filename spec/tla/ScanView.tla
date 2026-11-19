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
(* One transaction writes an intent in the range (008 FR-021): its entry is answered in the log     *)
(* and noted by nobody, since no read sees an intent as a version. Before its commit the           *)
(* transaction checks that the intent is applied, and with CheckNotes the range notes then the     *)
(* last entry it applied. The commit is answered in the range of the transaction's record, at a    *)
(* mark above the entries 1..commitPos of this range. Later an entry resolves the intent: it       *)
(* takes the intent away and writes the version at the commit's mark, below the marks the range    *)
(* gives by then. A scan reads the memtable the core writes to, at the mark of its view: it goes   *)
(* to the core if it meets the intent, and once the intent is resolved it sees the version only    *)
(* if its view is above commitPos. With ResolutionHeld a scan reads only a view that applied the   *)
(* resolution the range began to apply. CheckNotes = FALSE and ResolutionHeld = FALSE each         *)
(* violate ScansSeeAnswered: a view of before the intent, or one between the intent and the        *)
(* commit read after the resolution, shows nothing of a transaction committed before the scan.     *)
(* Invariant: a scan served holds every write answered before it began, and the transaction when   *)
(* its commit was answered before it began.                                                        *)
EXTENDS Naturals, FiniteSets

CONSTANTS MaxWrites, MaxScans, EntryInView, AnswersAfterViews, LooksAtAnswered, CheckNotes,
          ResolutionHeld

VARIABLES
    proposed,   \* entries in the leader's log: 1..proposed
    applied,    \* entries applied: 1..applied
    early,      \* entries answered before they were applied
    answered,   \* entries answered to their client
    noted,      \* the highest entry answered, as the range notes it
    published,  \* whether the core publishes a view of the range
    view,       \* the last entry applied in the view published
    scan,       \* a scan that began: the entry it took and the writes answered when it began
    served,     \* scans served: <<last entry of the view read, writes answered when it began,
                \* whether the transaction had committed when it began, whether it saw it>>
    intent,     \* the entry that wrote the transaction's intent, 0 before
    checked,    \* the transaction's check of its intent was answered
    committed,  \* the transaction's commit was answered to its client
    commitPos,  \* the commit's mark is above the entries 1..commitPos and below the later ones
    resolved    \* the entry that resolves the intent, 0 before it is applied

txn == <<intent, checked, committed, commitPos, resolved>>
vars == <<proposed, applied, early, answered, noted, published, view, scan, served, txn>>

NoScan == [on |-> FALSE, need |-> 0, must |-> {}, txn |-> FALSE]

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
    /\ intent = 0
    /\ checked = FALSE
    /\ committed = FALSE
    /\ commitPos = 0
    /\ resolved = 0

Propose ==
    /\ proposed < MaxWrites
    /\ proposed' = proposed + 1
    /\ UNCHANGED <<applied, early, answered, noted, published, view, scan, served, txn>>

Apply ==
    /\ applied < proposed
    /\ applied' = applied + 1
    /\ UNCHANGED <<proposed, early, answered, noted, published, view, scan, served, txn>>

\* The transaction writes its intent: the entry is answered in the log and nobody notes it.
PutIntent ==
    /\ intent = 0
    /\ proposed < MaxWrites
    /\ proposed' = proposed + 1
    /\ intent' = proposed + 1
    /\ UNCHANGED <<applied, early, answered, noted, published, view, scan, served, checked,
                   committed, commitPos, resolved>>

\* Its check is answered once the intent is applied; with CheckNotes the range notes what it has
\* applied by then.
Check ==
    /\ intent # 0
    /\ intent <= applied
    /\ ~checked
    /\ checked' = TRUE
    /\ noted' = IF CheckNotes /\ applied > noted THEN applied ELSE noted
    /\ UNCHANGED <<proposed, applied, early, answered, published, view, scan, served, intent,
                   committed, commitPos, resolved>>

\* Its commit is answered, in the range of its record: at a mark above the intent's, which the
\* entries this range proposed meanwhile may be above or below.
Commit ==
    /\ checked
    /\ ~committed
    /\ committed' = TRUE
    /\ commitPos' \in intent..proposed
    /\ UNCHANGED <<proposed, applied, early, answered, noted, published, view, scan, served,
                   intent, checked, resolved>>

\* An entry resolves the intent: the range notes it as it applies it.
Resolve ==
    /\ committed
    /\ resolved = 0
    /\ applied = proposed
    /\ proposed < MaxWrites
    /\ proposed' = proposed + 1
    /\ applied' = applied + 1
    /\ resolved' = proposed + 1
    /\ UNCHANGED <<early, answered, noted, published, view, scan, served, intent, checked,
                   committed, commitPos>>

\* The core publishes the view of what it has applied...
Publish ==
    /\ published' = TRUE
    /\ view' = applied
    /\ UNCHANGED <<proposed, applied, early, answered, noted, scan, served, txn>>

\* ...or none.
Withdraw ==
    /\ published
    /\ published' = FALSE
    /\ UNCHANGED <<proposed, applied, early, answered, noted, view, scan, served, txn>>

\* A write is answered before it is applied: the range notes its entry, then answers.
AnswerEarly ==
    /\ \E m \in (applied + 1)..proposed :
        /\ m \notin answered
        /\ m # intent
        /\ early' = early \cup {m}
        /\ answered' = answered \cup {m}
        /\ noted' = IF m > noted THEN m ELSE noted
    /\ UNCHANGED <<proposed, applied, published, view, scan, served, txn>>

\* A write is answered once applied; with AnswersAfterViews, after the core published a view
\* that holds it, or none.
AnswerApplied ==
    /\ \E m \in 1..applied :
        /\ m \notin answered
        /\ m # intent
        /\ AnswersAfterViews => (published => view >= m)
        /\ answered' = answered \cup {m}
        /\ noted' = IF m > noted THEN m ELSE noted
    /\ UNCHANGED <<proposed, applied, early, published, view, scan, served, txn>>

\* A scan begins: it takes the entry the range has noted, and has to see the writes answered so
\* far, and the transaction if its commit was answered.
ScanBegin ==
    /\ ~scan.on
    /\ Cardinality(served) < MaxScans
    /\ scan' = [on |-> TRUE, need |-> noted, must |-> answered, txn |-> committed]
    /\ UNCHANGED <<proposed, applied, early, answered, noted, published, view, served, txn>>

\* The scan meets the intent in the memtable: applied, not resolved, at or below its view's mark.
MeetsIntent == intent # 0 /\ intent <= applied /\ resolved = 0 /\ view >= intent

\* It reads the view published now if that view may serve it; otherwise it waits for another.
\* It sees the transaction when the version is written and its view is above the commit's mark.
ScanRead ==
    /\ scan.on
    /\ published
    /\ \/ ~LooksAtAnswered
       \/ LooksAtAnswered /\ EntryInView /\ view >= scan.need
       \/ LooksAtAnswered /\ ~EntryInView /\ {m \in early : m > applied} = {}
    /\ ~MeetsIntent
    /\ ResolutionHeld => view >= resolved
    /\ served' = served \cup {<<view, scan.must, scan.txn, resolved # 0 /\ view > commitPos>>}
    /\ scan' = NoScan
    /\ UNCHANGED <<proposed, applied, early, answered, noted, published, view, txn>>

\* It goes to the core instead.
ScanGivesUp ==
    /\ scan.on
    /\ scan' = NoScan
    /\ UNCHANGED <<proposed, applied, early, answered, noted, published, view, served, txn>>

Next == Propose \/ Apply \/ Publish \/ Withdraw \/ AnswerEarly \/ AnswerApplied
        \/ PutIntent \/ Check \/ Commit \/ Resolve
        \/ ScanBegin \/ ScanRead \/ ScanGivesUp

Spec == Init /\ [][Next]_vars

ScansSeeAnswered == \A s \in served : (\A m \in s[2] : m <= s[1]) /\ (s[3] => s[4])

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
    /\ intent \in 0..proposed
    /\ resolved \in 0..applied
    /\ commitPos \in 0..proposed
    /\ checked \in BOOLEAN
    /\ committed \in BOOLEAN
=============================================================================
