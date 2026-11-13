------------------------------ MODULE CoreRelocate ------------------------------
(* Moving a range to another core of every node in one coordinated step (007 FR-011).             *)
(* A range has the same core index on all its replicas. The move is an entry of the range's own  *)
(* log (RelocateRange to core c): each replica that applies it copies the range's state (its SST *)
(* and its unapplied log tail) to core c, then activates it there and drops the old copy. A      *)
(* replica that crashes during the move recovers from what is durable: if the copy on c is       *)
(* complete it activates c, otherwise it discards the partial copy and copies again from the old *)
(* core, which is only dropped after c is active.                                                *)
(* A replica that applies the move either stops at once, or goes on applying on its old core     *)
(* until its cut (linger): a repeat of the move in the log, another move, or a stop of its own   *)
(* (LocalCut). While it goes on it may export its state (Warm): the copy then is that export     *)
(* with the entries it applied since, which the target core applies as it opens the range        *)
(* (WithTail). A range that stopped for the whole export was served by no one meanwhile where it *)
(* has a single voter (H-059 of the native tests).                                               *)
(* The export is taken after the move is applied, so its state names the target core: a copy     *)
(* whose state names another core applies nothing on its own, and after a restart with two       *)
(* copies on the node the state tells the imported one. WarmBeforeMove = TRUE lets a replica     *)
(* export before it applied the move, and CopyNamesItsCore fails. WithTail = FALSE hands the     *)
(* export alone, and NoEntryLost fails.                                                          *)
(* Invariants: on each node at most one core runs the range, a running replica is on the core    *)
(* its applied prefix of the log names unless it waits for its cut, the running copy holds every *)
(* entry the replica applied, and a complete copy names the core it is on.                       *)
EXTENDS Integers, Sequences, FiniteSets

CONSTANTS Node, Core, Start, MaxLog, WithTail, WarmBeforeMove

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
    complete,   \* Node -> the copy on the target core is complete and durable
    warm,       \* Node -> entries the export it took while running holds (Absent: none)
    linger      \* Node -> it applied a move and goes on applying on its old core until its cut

vars == <<log, app, phase, core, target, held, complete, warm, linger>>

Init ==
    /\ log = <<>>
    /\ app = [n \in Node |-> 0]
    /\ phase = [n \in Node |-> "running"]
    /\ core = [n \in Node |-> Start]
    /\ target = [n \in Node |-> Start]
    /\ held = [n \in Node |-> [c \in Core |-> IF c = Start THEN 0 ELSE Absent]]
    /\ complete = [n \in Node |-> FALSE]
    /\ warm = [n \in Node |-> Absent]
    /\ linger = [n \in Node |-> FALSE]

\* The core named by the first i entries of the log: the last relocation among them, or Start.
RECURSIVE CoreAt(_)
CoreAt(i) == IF i = 0 THEN Start ELSE IF log[i] = Write THEN CoreAt(i - 1) ELSE log[i]

\* The leader appends a write or a relocation; a relocation to the core the log already names
\* is a repeat.
Propose(e) ==
    /\ Len(log) < MaxLog
    /\ log' = Append(log, e)
    /\ UNCHANGED <<app, phase, core, target, held, complete, warm, linger>>

\* A replica that does not wait for a cut applies entry e: a move stops it on its core, or it goes
\* on applying there until its cut; a repeat changes nothing.
ApplyPlain(n, e) ==
    /\ IF e = Write \/ e = CoreAt(app[n])
       THEN UNCHANGED <<phase, target, linger>>
       ELSE /\ target' = [target EXCEPT ![n] = e]
            /\ \/ phase' = [phase EXCEPT ![n] = "moving"] /\ UNCHANGED linger
               \/ linger' = [linger EXCEPT ![n] = TRUE] /\ UNCHANGED phase
    /\ UNCHANGED warm

\* A replica that waits for its cut applies entry e: a repeat of its move stops it with the export
\* it took; another move stops it too, or leaves it running if it names the core it is on, and its
\* export is void.
ApplyLingering(n, e) ==
    IF e = Write
    THEN UNCHANGED <<phase, target, linger, warm>>
    ELSE /\ linger' = [linger EXCEPT ![n] = FALSE]
         /\ target' = [target EXCEPT ![n] = e]
         /\ phase' = [phase EXCEPT ![n] = IF e = core[n] THEN "running" ELSE "moving"]
         /\ warm' = IF e = target[n] THEN warm ELSE [warm EXCEPT ![n] = Absent]

\* A running replica applies its next entry on its core.
Apply(n) ==
    /\ phase[n] = "running" /\ app[n] < Len(log)
    /\ app' = [app EXCEPT ![n] = @ + 1]
    /\ held' = [held EXCEPT ![n][core[n]] = @ + 1]
    /\ IF linger[n]
       THEN ApplyLingering(n, log[app[n] + 1])
       ELSE ApplyPlain(n, log[app[n] + 1])
    /\ UNCHANGED <<log, core, complete>>

\* A replica that waits for its cut stops on its own: one that does not lead, once it has its
\* files.
LocalCut(n) ==
    /\ phase[n] = "running" /\ linger[n]
    /\ phase' = [phase EXCEPT ![n] = "moving"]
    /\ linger' = [linger EXCEPT ![n] = FALSE]
    /\ UNCHANGED <<log, app, core, target, held, complete, warm>>

\* A replica that goes on applying after its move exports its state as it is now.
Warm(n) ==
    /\ phase[n] = "running" /\ warm[n] = Absent
    /\ linger[n] \/ WarmBeforeMove
    /\ warm' = [warm EXCEPT ![n] = app[n]]
    /\ UNCHANGED <<log, app, phase, core, target, held, complete, linger>>

\* The copy to the target core finishes and is made durable: the export taken while running, or
\* the old core's state.
Copy(n) ==
    /\ phase[n] = "moving" /\ ~complete[n]
    /\ held' = [held EXCEPT ![n][target[n]] =
                    IF warm[n] /= Absent THEN warm[n] ELSE held[n][core[n]]]
    /\ complete' = [complete EXCEPT ![n] = TRUE]
    /\ warm' = [warm EXCEPT ![n] = Absent]
    /\ UNCHANGED <<log, app, phase, core, target, linger>>

\* The target core starts the range, applies the entries that followed its copy, and the old copy
\* is dropped.
Activate(n) ==
    /\ phase[n] = "moving" /\ complete[n]
    /\ phase' = [phase EXCEPT ![n] = "running"]
    /\ core' = [core EXCEPT ![n] = target[n]]
    /\ held' = [held EXCEPT ![n][core[n]] = Absent,
                            ![n][target[n]] = IF WithTail THEN app[n] ELSE @]
    /\ complete' = [complete EXCEPT ![n] = FALSE]
    /\ UNCHANGED <<log, app, target, warm, linger>>

\* A crash loses what is not durable: a partial copy on the target core, an export taken while
\* running, and the wait for a cut.
Crash(n) ==
    /\ phase[n] /= "down"
    /\ phase' = [phase EXCEPT ![n] = "down"]
    /\ held' = IF phase[n] = "moving" /\ ~complete[n]
               THEN [held EXCEPT ![n][target[n]] = Absent] ELSE held
    /\ warm' = [warm EXCEPT ![n] = Absent]
    /\ linger' = [linger EXCEPT ![n] = FALSE]
    /\ UNCHANGED <<log, app, core, target, complete>>

\* Recovery: a replica whose applied prefix names another core resumes the move; a complete copy
\* on the target is activated, otherwise it copies again from the old core. One that had not made
\* durable what it applied after its move finds it in its log and may wait for its cut again.
Recover(n) ==
    /\ phase[n] = "down"
    /\ IF CoreAt(app[n]) = core[n]
       THEN /\ phase' = [phase EXCEPT ![n] = "running"]
            /\ UNCHANGED <<target, linger>>
       ELSE /\ target' = [target EXCEPT ![n] = CoreAt(app[n])]
            /\ \/ phase' = [phase EXCEPT ![n] = "moving"] /\ UNCHANGED linger
               \/ /\ ~complete[n]
                  /\ phase' = [phase EXCEPT ![n] = "running"]
                  /\ linger' = [linger EXCEPT ![n] = TRUE]
    /\ UNCHANGED <<log, app, core, held, complete, warm>>

Next ==
    \/ \E e \in Core \cup {Write} : Propose(e)
    \/ \E n \in Node :
        \/ Apply(n) \/ LocalCut(n) \/ Warm(n) \/ Copy(n) \/ Activate(n)
        \/ Crash(n) \/ Recover(n)

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ \A n \in Node : phase[n] \in {"running", "moving", "down"} /\ core[n] \in Core
    /\ \A n \in Node : linger[n] \in BOOLEAN /\ target[n] \in Core

Running(n) == phase[n] = "running"
SingleOwner == \A n \in Node : Running(n) => {c \in Core : held[n][c] /= Absent} = {core[n]}
CoreMatchesLog ==
    \A n \in Node : Running(n) =>
        IF linger[n]
        THEN CoreAt(app[n]) = target[n] /\ target[n] /= core[n]
        ELSE core[n] = CoreAt(app[n])
SameIndexSameCore ==
    \A n, m \in Node :
        Running(n) /\ Running(m) /\ ~linger[n] /\ ~linger[m] /\ app[n] = app[m]
            => core[n] = core[m]
NoEntryLost == \A n \in Node : phase[n] /= "down" => held[n][core[n]] = app[n]
CopyNamesItsCore == \A n \in Node : complete[n] => CoreAt(held[n][target[n]]) = target[n]
=============================================================================
