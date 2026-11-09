----------------------------- MODULE PipelinedWrites -----------------------------
(* Writes in a row to one document on its range's leader (005 FR-029, T051, H-008b of the native   *)
(* tests). Each write is an increment: its value is the value it was evaluated on plus one, so     *)
(* the value of a confirmed write says on which writes it was built. A write takes the write latch, *)
(* is evaluated on the newest value the leader has, pending or applied, and gives the latch back   *)
(* once its entry is in the leader's log, before it commits: the next write to the document is     *)
(* evaluated on that pending value. A proposal the leader never writes to its log (refused,        *)
(* dropped) gives the latch back with its value withdrawn. A leader that loses its term takes with *)
(* it every entry not committed, which Raft truncates as a suffix of the log (log matching), and   *)
(* the writes evaluated on them.                                                                   *)
(* Reads wait while any write of the document is pending (proposed and not applied), then see the *)
(* committed value.                                                                                *)
(* ReleaseAtAppend and ReadsWait turn each defence off: releasing the latch when the write is      *)
(* evaluated (before its entry is in the log), and reads that see pending values: without the     *)
(* first CommittedInOrder fails (09-11-2026, depth 10), without the second ReadsCommitted (depth 8). *)
(* FailuresWait turns off the wait of a failed evaluation (ReadsCommitted fails at depth 7).       *)
(* Invariants: the committed values are 1, 2, 3, ... (no confirmed write was built on a lost one), *)
(* every write acknowledged stays committed, and no read saw a value that was not committed.       *)
EXTENDS Naturals, Sequences

CONSTANTS MaxWrites, MaxLosses, ReleaseAtAppend, ReadsWait, FailuresWait

VARIABLES
    log,        \* the values of the leader's log, in order
    commit,     \* entries committed (and applied)
    latch,      \* a write holds the write latch
    evaluated,  \* writes evaluated whose entry is not in the log yet: their values, in order
    newest,     \* the newest value a write is evaluated on: log, pending and evaluated
    acks,       \* values of the writes acknowledged to their clients
    reads,      \* values the reads saw
    writes,     \* writes started
    losses      \* leaderships lost

vars == <<log, commit, latch, evaluated, newest, acks, reads, writes, losses>>

Init ==
    /\ log = <<>>
    /\ commit = 0
    /\ latch = FALSE
    /\ evaluated = <<>>
    /\ newest = 0
    /\ acks = {}
    /\ reads = {}
    /\ writes = 0
    /\ losses = 0

\* A write takes the latch and is evaluated on the newest value. Released at append, the latch
\* stays held; released at evaluation (the faulty variant), it is free again at once.
Eval ==
    /\ writes < MaxWrites
    /\ ~latch
    /\ writes' = writes + 1
    /\ evaluated' = Append(evaluated, newest + 1)
    /\ newest' = newest + 1
    /\ latch' = ReleaseAtAppend
    /\ UNCHANGED <<log, commit, acks, reads, losses>>

\* The oldest evaluated write's entry goes to the leader's log; its latch is given back.
AppendEntry ==
    /\ evaluated # <<>>
    /\ log' = Append(log, Head(evaluated))
    /\ evaluated' = Tail(evaluated)
    /\ latch' = FALSE
    /\ UNCHANGED <<commit, newest, acks, reads, writes, losses>>

\* The oldest evaluated write never reaches the log (its proposal is refused): its value is
\* withdrawn, and the newest value goes back to what the log and the other evaluated writes hold.
Drop ==
    /\ evaluated # <<>>
    /\ evaluated' = Tail(evaluated)
    /\ newest' = IF Tail(evaluated) # <<>>
                    THEN evaluated[Len(evaluated)]
                    ELSE IF log # <<>> THEN log[Len(log)] ELSE 0
    /\ latch' = FALSE
    /\ UNCHANGED <<log, commit, acks, reads, writes, losses>>

\* The next entry commits and is applied; its client gets its answer.
Commit ==
    /\ commit < Len(log)
    /\ commit' = commit + 1
    /\ acks' = acks \cup {log[commit + 1]}
    /\ UNCHANGED <<log, latch, evaluated, newest, reads, writes, losses>>

\* The leader loses its term: the entries not committed and the writes evaluated go.
Lose ==
    /\ losses < MaxLosses
    /\ losses' = losses + 1
    /\ log' = SubSeq(log, 1, commit)
    /\ evaluated' = <<>>
    /\ newest' = IF commit > 0 THEN log[commit] ELSE 0
    /\ latch' = FALSE
    /\ UNCHANGED <<commit, acks, reads, writes>>

\* A read waits while a write of the document is pending, then sees the committed value; in the
\* faulty variant it sees the newest value in the log at once.
Read ==
    /\ IF ReadsWait THEN Len(log) = commit /\ evaluated = <<>> ELSE TRUE
    /\ reads' = reads \cup {IF ReadsWait \/ log = <<>> THEN commit ELSE log[Len(log)]}
    /\ UNCHANGED <<log, commit, latch, evaluated, newest, acks, writes, losses>>

\* A write whose evaluation fails (a compare-and-swap that finds another value) answers what it
\* found: it is a read. It waits, as a read does, for the pending writes before it answers; in the
\* faulty variant it answers on the newest pending value (the simulator found this one, 09-11-2026:
\* a compare-and-swap failed on a write the leader then lost).
Fail ==
    /\ IF FailuresWait THEN Len(log) = commit /\ evaluated = <<>> ELSE TRUE
    /\ reads' = reads \cup {IF FailuresWait THEN commit ELSE newest}
    /\ UNCHANGED <<log, commit, latch, evaluated, newest, acks, writes, losses>>

Next == Eval \/ AppendEntry \/ Drop \/ Commit \/ Lose \/ Read \/ Fail

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ commit \in 0..Len(log)
    /\ latch \in BOOLEAN
    /\ writes \in 0..MaxWrites
    /\ losses \in 0..MaxLosses

\* The committed values are 1, 2, 3, ...: every confirmed write was built on the confirmed ones.
CommittedInOrder == \A i \in 1..commit : log[i] = i

\* An acknowledged write is never lost.
AcksStay == \A a \in acks : a <= commit

\* A read saw only what was committed.
ReadsCommitted == \A r \in reads : r <= commit
================================================================================
