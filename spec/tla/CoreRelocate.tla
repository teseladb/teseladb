------------------------------ MODULE CoreRelocate ------------------------------
(* Moving a range to another core of every node in one coordinated step (007 FR-011).             *)
(* A range has the same core index on all its replicas. The move is an entry of the range's own  *)
(* log (RelocateRange to core c): each replica that applies it stops the range on its old core,  *)
(* copies the range's state (its SST and its unapplied log tail) to core c, then activates it   *)
(* there and drops the old copy. A replica that crashes during the move recovers from what is   *)
(* durable: if the copy on c is complete it activates c, otherwise it discards the partial copy  *)
(* and copies again from the old core, which is only dropped after c is active. Entries before  *)
(* the move are applied on the old core and entries after it on the new one.                     *)
(* Invariants: on each node at most one core runs the range, a running replica is on the core   *)
(* its applied prefix of the log names (so two replicas at the same index agree on the core),   *)
(* and the running copy holds every entry the replica applied.                                   *)
EXTENDS Integers, Sequences, FiniteSets

CONSTANTS Node, Core, Start, MaxLog

ASSUME Start \in Core
Write == -2  \* a log entry that is not a relocation
Absent == -1

VARIABLES
    log,        \* the range's committed log: Write or a target core
    app,        \* Node -> entries applied
    phase,      \* Node -> "running" | "moving" | "down"
    core,       \* Node -> the core the replica runs on (or moves from)
    target,     \* Node -> the core it moves to
    held,       \* Node -> Core -> entries held by that core's copy (Absent: no copy)
    complete    \* Node -> the copy on the target core is complete and durable

vars == <<log, app, phase, core, target, held, complete>>

Init ==
    /\ log = <<>>
    /\ app = [n \in Node |-> 0]
    /\ phase = [n \in Node |-> "running"]
    /\ core = [n \in Node |-> Start]
    /\ target = [n \in Node |-> Start]
    /\ held = [n \in Node |-> [c \in Core |-> IF c = Start THEN 0 ELSE Absent]]
    /\ complete = [n \in Node |-> FALSE]

\* The core named by the first i entries of the log: the last relocation among them, or Start.
RECURSIVE CoreAt(_)
CoreAt(i) == IF i = 0 THEN Start ELSE IF log[i] = Write THEN CoreAt(i - 1) ELSE log[i]

\* The leader appends a write or a relocation to a core other than the current one.
Propose(e) ==
    /\ Len(log) < MaxLog
    /\ e /= Write => e /= CoreAt(Len(log))
    /\ log' = Append(log, e)
    /\ UNCHANGED <<app, phase, core, target, held, complete>>

\* A running replica applies its next entry on its core; a relocation stops it there.
Apply(n) ==
    /\ phase[n] = "running" /\ app[n] < Len(log)
    /\ app' = [app EXCEPT ![n] = @ + 1]
    /\ held' = [held EXCEPT ![n][core[n]] = @ + 1]
    /\ IF log[app[n] + 1] = Write
       THEN UNCHANGED <<phase, target>>
       ELSE /\ phase' = [phase EXCEPT ![n] = "moving"]
            /\ target' = [target EXCEPT ![n] = log[app[n] + 1]]
    /\ UNCHANGED <<log, core, complete>>

\* The copy to the target core finishes and is made durable.
Copy(n) ==
    /\ phase[n] = "moving" /\ ~complete[n]
    /\ held' = [held EXCEPT ![n][target[n]] = held[n][core[n]]]
    /\ complete' = [complete EXCEPT ![n] = TRUE]
    /\ UNCHANGED <<log, app, phase, core, target>>

\* The target core starts the range and the old copy is dropped.
Activate(n) ==
    /\ phase[n] = "moving" /\ complete[n]
    /\ phase' = [phase EXCEPT ![n] = "running"]
    /\ core' = [core EXCEPT ![n] = target[n]]
    /\ held' = [held EXCEPT ![n][core[n]] = Absent]
    /\ complete' = [complete EXCEPT ![n] = FALSE]
    /\ UNCHANGED <<log, app, target>>

\* A crash loses what is not durable: a partial copy on the target core.
Crash(n) ==
    /\ phase[n] /= "down"
    /\ phase' = [phase EXCEPT ![n] = "down"]
    /\ held' = IF phase[n] = "moving" /\ ~complete[n]
               THEN [held EXCEPT ![n][target[n]] = Absent] ELSE held
    /\ UNCHANGED <<log, app, core, target, complete>>

\* Recovery: a replica whose applied prefix ends in a relocation resumes the move; a complete
\* copy on the target is activated, otherwise it copies again from the old core.
Recover(n) ==
    /\ phase[n] = "down"
    /\ phase' = [phase EXCEPT ![n] = IF CoreAt(app[n]) = core[n] THEN "running" ELSE "moving"]
    /\ UNCHANGED <<log, app, core, target, held, complete>>

Next ==
    \/ \E e \in Core \cup {Write} : Propose(e)
    \/ \E n \in Node : Apply(n) \/ Copy(n) \/ Activate(n) \/ Crash(n) \/ Recover(n)

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ \A n \in Node : phase[n] \in {"running", "moving", "down"} /\ core[n] \in Core

Running(n) == phase[n] = "running"
SingleOwner == \A n \in Node : Running(n) => {c \in Core : held[n][c] /= Absent} = {core[n]}
CoreMatchesLog == \A n \in Node : Running(n) => core[n] = CoreAt(app[n])
SameIndexSameCore ==
    \A n, m \in Node : Running(n) /\ Running(m) /\ app[n] = app[m] => core[n] = core[m]
NoEntryLost == \A n \in Node : phase[n] /= "down" => held[n][core[n]] = app[n]
=============================================================================
