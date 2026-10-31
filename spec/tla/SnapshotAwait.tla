------------------------------ MODULE SnapshotAwait ------------------------------
(* A follower whose state machine waits for a snapshot (007 T016, D-067).                         *)
(* The cluster commits entries 1..committed of one log. A follower keeps a log whose entries up   *)
(* to `good` are the committed ones and whose entries good+1..len came from a deposed leader.    *)
(* Its Raft layer hands entries out to apply up to its commit index; its state machine holds the *)
(* effects of entries 1..sm. A restart keeps the state machine's durable effects (an LSM flushed *)
(* up to some index at or above the snapshot) or loses them: then the replica waits for a        *)
(* snapshot. While it waits it keeps voting, appending and committing, but nothing is handed out *)
(* to apply; it gets its own snapshot's data back, or installs a leader's snapshot, also one at  *)
(* or below its commit index: keeping the log past it when the log has it, or replacing the log *)
(* when the snapshot is at or past the commit index. After a restart that kept the durable      *)
(* effects, the state machine skips the entries they already hold.                               *)
(* A leader's snapshot that replaces the log is taken by Raft in memory first (its commit index   *)
(* goes to the snapshot's) and is durable later; meanwhile a newer one may replace it, and any    *)
(* other Ready may write the hard state. With CapCommit that hard state carries at most the      *)
(* commit index on disk when the snapshot came (005, seed 4058): without it a restart before the  *)
(* snapshot is durable reads back a commit index past the old log, whose entries past `good`     *)
(* were never committed.                                                                          *)
(* Invariants: the state machine holds a contiguous prefix of the committed entries and nothing  *)
(* else, the commit index never covers an entry the cluster did not commit, and a replica that  *)
(* waits for a snapshot hands nothing out past its snapshot.                                    *)
EXTENDS Integers

CONSTANTS MaxLog, MaxRestarts, CapCommit

Unrestored == -1

VARIABLES
    committed,  \* entries the cluster committed
    good,       \* the follower's log holds the committed entries 1..good
    len,        \* last index of the follower's log (good+1..len: a deposed leader's entries)
    snap,       \* index of the follower's durable snapshot (its log starts after it)
    commit,     \* the follower's commit index
    handed,     \* last entry handed out to apply
    sm,         \* the state machine holds the effects of 1..sm (Unrestored: not restored yet)
    smOk,       \* every entry the state machine applied was the next committed one
    waiting,    \* the state machine waits for a snapshot
    restarts,
    pend,       \* a leader's snapshot Raft took in memory that is not durable yet (0: none)
    dcommit     \* the commit index in the hard state on disk

vars == <<committed, good, len, snap, commit, handed, sm, smOk, waiting, restarts, pend, dcommit>>

Max(a, b) == IF a > b THEN a ELSE b
Min(a, b) == IF a < b THEN a ELSE b

Init ==
    /\ committed = 0
    /\ good = 0
    /\ len = 0
    /\ snap = 0
    /\ commit = 0
    /\ handed = 0
    /\ sm = 0
    /\ smOk = TRUE
    /\ waiting = FALSE
    /\ restarts = 0
    /\ pend = 0
    /\ dcommit = 0

TypeOK ==
    /\ committed \in 0..MaxLog
    /\ good \in 0..MaxLog
    /\ len \in 0..MaxLog
    /\ snap \in 0..MaxLog
    /\ commit \in 0..MaxLog
    /\ handed \in 0..MaxLog
    /\ sm \in {Unrestored} \cup 0..MaxLog
    /\ smOk \in BOOLEAN
    /\ waiting \in BOOLEAN
    /\ restarts \in 0..MaxRestarts
    /\ pend \in 0..MaxLog
    /\ dcommit \in 0..MaxLog

\* The cluster commits one more entry.
Commit ==
    /\ committed < MaxLog
    /\ committed' = committed + 1
    /\ UNCHANGED <<good, len, snap, commit, handed, sm, smOk, waiting, restarts, pend, dcommit>>

\* The leader's entries up to k, after a matching entry: a conflicting suffix is dropped.
Append(k) ==
    /\ pend = 0
    /\ k <= committed
    /\ k > good
    /\ good' = k
    /\ len' = IF len > good THEN k ELSE Max(len, k)
    /\ UNCHANGED <<committed, snap, commit, handed, sm, smOk, waiting, restarts, pend, dcommit>>

\* A deposed leader appends an entry the cluster never commits.
Diverge ==
    /\ pend = 0
    /\ len < MaxLog
    /\ len' = len + 1
    /\ UNCHANGED <<committed, good, snap, commit, handed, sm, smOk, waiting, restarts, pend, dcommit>>

\* The leader's commit index, never past what the follower has in common with it.
LearnCommit ==
    /\ pend = 0
    /\ commit < Min(committed, good)
    /\ commit' = Min(committed, good)
    /\ UNCHANGED <<committed, good, len, snap, handed, sm, smOk, waiting, restarts, pend, dcommit>>

\* The next committed entry is handed out and applied, or skipped if the durable effects hold it.
Apply ==
    /\ pend = 0
    /\ ~waiting
    /\ handed < commit
    /\ handed' = handed + 1
    /\ IF handed + 1 <= sm
         THEN UNCHANGED <<sm, smOk>>
         ELSE /\ sm' = handed + 1
              /\ smOk' = (smOk /\ sm = handed /\ handed + 1 <= good)
    /\ UNCHANGED <<committed, good, len, snap, commit, waiting, restarts, pend, dcommit>>

\* A snapshot of the state machine compacts the log up to an applied entry.
TakeSnapshot ==
    /\ pend = 0
    /\ ~waiting
    /\ sm > snap
    /\ sm <= handed
    /\ snap' = sm
    /\ UNCHANGED <<committed, good, len, commit, handed, sm, smOk, waiting, restarts, pend, dcommit>>

\* The hard state of a Ready is written with Raft's commit index (capped while a leader's snapshot
\* is not durable, with CapCommit).
PersistHS ==
    /\ dcommit' = IF pend # 0 /\ CapCommit THEN dcommit ELSE commit
    /\ dcommit' # dcommit
    /\ UNCHANGED <<committed, good, len, snap, commit, handed, sm, smOk, waiting, restarts, pend>>

\* A restart: the commit index read back is the one on disk (not below the snapshot); a leader's
\* snapshot that was not durable is lost; the state machine keeps durable effects up to some
\* index from the snapshot up to what it applied, or lost them and waits.
Restart ==
    /\ restarts < MaxRestarts
    /\ restarts' = restarts + 1
    /\ handed' = snap
    /\ commit' = Max(dcommit, snap)
    /\ pend' = 0
    /\ \/ /\ sm # Unrestored
          /\ sm' \in snap..Max(snap, sm)
          /\ waiting' = FALSE
       \/ /\ sm' = Unrestored
          /\ waiting' = TRUE
    /\ UNCHANGED <<committed, good, len, snap, smOk, dcommit>>

\* The data of its own snapshot arrives: the state machine is restored at it.
OwnData ==
    /\ pend = 0
    /\ waiting
    /\ sm' = snap
    /\ waiting' = FALSE
    /\ UNCHANGED <<committed, good, len, snap, commit, handed, smOk, restarts, pend, dcommit>>

\* A leader's snapshot at index i, with its data. One that replaces the log goes to Raft's memory
\* first (`pend`) and is made durable by `Durable`; a newer one replaces one not durable yet.
Install(i) ==
    /\ i \in 1..committed
    /\ LET awaited == waiting /\ i > handed
           \* The log has entry i (below the follower's commit index it is a committed one).
           matches == i > snap /\ i <= good
       IN
       \/ /\ pend # 0
          /\ i > pend
          /\ pend' = i /\ commit' = i
          /\ UNCHANGED <<good, len, snap, handed, sm, waiting>>
       \/ /\ pend = 0
          /\ ~awaited
          /\ i > commit
          /\ IF matches
               THEN /\ commit' = i
                    /\ UNCHANGED <<good, len, snap, handed, sm, waiting, pend>>
               ELSE /\ pend' = i /\ commit' = i
                    /\ UNCHANGED <<good, len, snap, handed, sm, waiting>>
       \/ /\ pend = 0
          /\ awaited
          /\ IF matches
               \* The log stays past it; the commit index does not go back.
               THEN /\ snap' = i /\ handed' = i /\ sm' = i /\ waiting' = FALSE
                    /\ commit' = Max(commit, i)
                    /\ UNCHANGED <<good, len, pend>>
               ELSE /\ i >= commit
                    /\ pend' = i /\ commit' = i
                    /\ UNCHANGED <<good, len, snap, handed, sm, waiting>>
    /\ UNCHANGED <<committed, smOk, restarts, dcommit>>

\* The leader's snapshot Raft took in memory is durable: it replaces the log and restores the
\* state machine, and the hard state of its Ready is written with it.
Durable ==
    /\ pend # 0
    /\ good' = pend /\ len' = pend /\ snap' = pend /\ handed' = pend /\ sm' = pend
    /\ waiting' = FALSE
    /\ dcommit' = commit
    /\ pend' = 0
    /\ UNCHANGED <<committed, commit, smOk, restarts>>

Next ==
    \/ Commit
    \/ \E k \in 1..MaxLog : Append(k)
    \/ Diverge
    \/ LearnCommit
    \/ Apply
    \/ TakeSnapshot
    \/ Restart
    \/ OwnData
    \/ \E i \in 1..MaxLog : Install(i)
    \/ Durable
    \/ PersistHS

Spec == Init /\ [][Next]_vars

\* The state machine holds a contiguous prefix of the committed entries.
StateMachineSafe == smOk /\ (sm # Unrestored => sm <= committed)

\* The commit index covers only committed entries the log has (Raft's memory may be ahead of the
\* log while a leader's snapshot is not durable), and the one on disk never covers an entry of the
\* log the cluster did not commit.
CommitSafe ==
    /\ pend = 0 => commit <= good
    /\ commit <= committed /\ good <= len
    /\ dcommit <= Max(good, pend)

\* A replica waiting for a snapshot hands nothing out past it.
NothingHandedWhileWaiting == waiting => (handed = snap /\ sm = Unrestored)

\* Handed-out entries are never past the commit index, and the log starts after the snapshot.
Ordered == snap <= handed /\ handed <= Max(commit, snap) /\ snap <= good
=============================================================================
