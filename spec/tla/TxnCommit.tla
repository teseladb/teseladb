------------------------------- MODULE TxnCommit -------------------------------
(* Distributed transactions (008 FR-001 to FR-007, FR-015, contract txn-protocol.md).             *)
(* Each transaction has a coordinator that takes a read timestamp, creates its record (Pending)    *)
(* and writes intents on its keys; the first writer wins: an intent of another live transaction or *)
(* a committed version newer than the read timestamp fails the write (WriteConflict). The leader   *)
(* of a key gives an intent a timestamp above every read it served on that key (005 FR-025).       *)
(* EndTxn(Committed) sets the commit timestamp to the maximum of the proposed one, the record's    *)
(* min_commit_ts and the intents' timestamps. A reader at t that finds a pending intent at or      *)
(* below t pushes the record's min_commit_ts above t (PushTxn, replicated): a missing record of a  *)
(* live transaction is created Pending with that min_commit_ts, the coordinator adopts it; a        *)
(* transaction without heartbeat is aborted, its record created Aborted if missing, and a late      *)
(* coordinator cannot create it any more. The reader then reads the version before the intent, or *)
(* the intent's value if the record is Committed at or below t. Anyone resolves the intents of a   *)
(* finished record: versions at the commit timestamp, or nothing. The coordinator can die at any  *)
(* point; the reaper aborts what has no heartbeat.                                                 *)
(* Invariants: no read served at t misses a transaction that commits at or below t (so readers      *)
(* never wait and later commits land above them), a transaction aborted for a reader never commits *)
(* (one outcome), first committer wins on a key (snapshot isolation), and only committed           *)
(* transactions leave versions, all at their commit timestamp.                                     *)
(* Mutant (cfg constant) replaces one rule by its faulty form, to check that the invariants catch  *)
(* it (008 FR-016, T024, T030): "none", "reader_ignores_intent" (no push, the reader reads below  *)
(* the intent), "commit_below_push" (the commit ignores min_commit_ts), "late_coordinator_creates" *)
(* (a coordinator creates its record over an Aborted one), "resolve_before_commit" (an intent      *)
(* becomes a version before EndTxn), and for T035 "begin_ignores_push" (a coordinator that finds   *)
(* the Pending record a push created starts it afresh, dropping its min_commit_ts) and             *)
(* "commit_below_intent" (the commit timestamp ignores the intents' timestamps).                   *)
(* With PushByRead (008 T043) a reader pushes a live transaction by reading its record: the read    *)
(* raises the read cache of the record's key to its timestamp (recRead) and leaves the record as it *)
(* is, and EndTxn(Committed) goes above that cache too. Pushing an abandoned transaction still      *)
(* writes the record. The reads are lease reads at or below the leader's clock (HlcLease covers     *)
(* them), so one clock is enough here. Mutant "commit_below_record_reads" ignores recRead.         *)
EXTENDS Naturals, FiniteSets

CONSTANTS Txns, Keys, MaxTs, MaxReads, Mutant, PushByRead

NoTxn == 0
None == "none"
Pending == "pending"
Committed == "committed"
Aborted == "aborted"

\* Coordinator phases.
Idle == "idle"
Writing == "writing"
Done == "done"
Dead == "dead"

VARIABLES
    now,       \* the HLC (one clock: 008 HlcUncertainty models the skew)
    coord,     \* coordinator phase of each transaction (Dead is not used: a stall that never
               \* resumes is a dead coordinator)
    readTs,    \* read timestamp of each transaction
    rec,       \* the record: [st, minC, cts]
    alive,     \* heartbeat of each transaction's coordinator
    wrote,     \* keys with an intent of the transaction
    intent,    \* intent on each key: [t, ts]
    ver,       \* committed versions of each key: set of [t, ts]
    maxRead,   \* the highest timestamp a read was served at, per key (the read cache)
    recRead,   \* the read cache of each transaction's record key: the highest push by a read
    reads,     \* ghost: reads served: [k, rts, seen]
    abortedFor, \* ghost: transactions a reader saw aborted
    broken     \* ghost: a read missed a commit at or below its timestamp

vars == <<now, coord, readTs, rec, alive, wrote, intent, ver, maxRead, recRead, reads, abortedFor, broken>>

Max(a, b) == IF a > b THEN a ELSE b
NoIntent == [t |-> NoTxn, ts |-> 0]

TypeOK ==
    /\ now \in 1..MaxTs
    /\ coord \in [Txns -> {Idle, Writing, Done, Dead}]
    /\ readTs \in [Txns -> 0..MaxTs]
    /\ rec \in [Txns -> [st : {None, Pending, Committed, Aborted}, minC : 0..MaxTs + 1, cts : 0..MaxTs + 1]]
    /\ alive \in [Txns -> BOOLEAN]
    /\ wrote \in [Txns -> SUBSET Keys]
    /\ intent \in [Keys -> [t : Txns \cup {NoTxn}, ts : 0..MaxTs + 1]]
    /\ maxRead \in [Keys -> 0..MaxTs]
    /\ recRead \in [Txns -> 0..MaxTs]
    /\ broken \in BOOLEAN

Init ==
    /\ now = 1
    /\ coord = [t \in Txns |-> Idle]
    /\ readTs = [t \in Txns |-> 0]
    /\ rec = [t \in Txns |-> [st |-> None, minC |-> 0, cts |-> 0]]
    /\ alive = [t \in Txns |-> TRUE]
    /\ wrote = [t \in Txns |-> {}]
    /\ intent = [k \in Keys |-> NoIntent]
    /\ ver = [k \in Keys |-> {}]
    /\ maxRead = [k \in Keys |-> 0]
    /\ recRead = [t \in Txns |-> 0]
    /\ reads = {}
    /\ abortedFor = {}
    /\ broken = FALSE

Tick ==
    /\ now < MaxTs
    /\ now' = now + 1
    /\ UNCHANGED <<coord, readTs, rec, alive, wrote, intent, ver, maxRead, recRead, reads, abortedFor, broken>>

Begin(t) ==
    /\ coord[t] = Idle
    /\ coord' = [coord EXCEPT ![t] = Writing]
    /\ readTs' = [readTs EXCEPT ![t] = now]
    /\ UNCHANGED <<now, rec, alive, wrote, intent, ver, maxRead, recRead, reads, abortedFor, broken>>

\* The coordinator creates its record: Pending, or it adopts the one a push created; over an
\* Aborted record it fails with NoSuchTransaction (the mutant overwrites it).
CreateRecord(t) ==
    /\ coord[t] = Writing /\ alive[t]
    /\ \/ /\ rec[t].st = None
          /\ rec' = [rec EXCEPT ![t].st = Pending]
          /\ UNCHANGED coord
       \/ /\ rec[t].st = Aborted
          /\ IF Mutant = "late_coordinator_creates"
                THEN /\ rec' = [rec EXCEPT ![t] = [st |-> Pending, minC |-> 0, cts |-> 0]]
                     /\ UNCHANGED coord
                ELSE /\ coord' = [coord EXCEPT ![t] = Done]
                     /\ UNCHANGED rec
       \/ /\ rec[t].st = Pending /\ rec[t].minC > 0 /\ Mutant = "begin_ignores_push"
          /\ rec' = [rec EXCEPT ![t].minC = 0]
          /\ UNCHANGED coord
    /\ UNCHANGED <<now, readTs, alive, wrote, intent, ver, maxRead, recRead, reads, abortedFor, broken>>

\* PutIntents on key k: a conflict ends the transaction (the driver retries it).
PutIntent(t, k) ==
    /\ coord[t] = Writing /\ alive[t] /\ k \notin wrote[t]
    /\ LET other == intent[k].t
           conflict == (other # NoTxn /\ other # t)
                       \/ \E v \in ver[k] : v.ts > readTs[t]
       IN IF conflict
             THEN /\ coord' = [coord EXCEPT ![t] = Done]
                  /\ rec' = [rec EXCEPT ![t].st = IF @ = Pending \/ @ = None THEN Aborted ELSE @]
                  /\ UNCHANGED <<wrote, intent>>
             ELSE /\ intent' = [intent EXCEPT ![k] = [t |-> t, ts |-> Max(now, maxRead[k] + 1)]]
                  /\ wrote' = [wrote EXCEPT ![t] = @ \cup {k}]
                  /\ UNCHANGED <<coord, rec>>
    /\ UNCHANGED <<now, readTs, alive, ver, maxRead, recRead, reads, abortedFor, broken>>

\* EndTxn(Committed) in the record's range.
Commit(t) ==
    /\ coord[t] = Writing /\ alive[t] /\ wrote[t] # {} /\ rec[t].st = Pending
    /\ LET intentMax == Max(IF \E k \in Keys : intent[k].t = t
                               THEN CHOOSE m \in 0..MaxTs + 1 :
                                        /\ \E k \in Keys : intent[k].t = t /\ intent[k].ts = m
                                        /\ \A k \in Keys : intent[k].t = t => intent[k].ts <= m
                               ELSE 0, now)
           readFloor == IF recRead[t] > 0 THEN recRead[t] + 1 ELSE 0
           cts == CASE Mutant = "commit_below_push" -> intentMax
                    [] Mutant = "commit_below_intent" -> Max(now, rec[t].minC)
                    [] Mutant = "commit_below_record_reads" -> Max(intentMax, rec[t].minC)
                    [] OTHER -> Max(Max(intentMax, rec[t].minC), readFloor)
       IN /\ cts <= MaxTs + 1
          /\ rec' = [rec EXCEPT ![t] = [@ EXCEPT !.st = Committed, !.cts = cts]]
          /\ coord' = [coord EXCEPT ![t] = Done]
          /\ broken' = (\/ broken
                         \/ (\E r \in reads : r.k \in wrote[t] /\ r.rts >= cts /\ t \notin r.seen)
                         \/ t \in abortedFor)
    /\ UNCHANGED <<now, readTs, alive, wrote, intent, ver, maxRead, recRead, reads, abortedFor>>

\* EndTxn(Aborted) by the coordinator.
Abort(t) ==
    /\ coord[t] = Writing /\ alive[t]
    /\ rec' = [rec EXCEPT ![t].st = IF @ = Pending \/ @ = None THEN Aborted ELSE @]
    /\ coord' = [coord EXCEPT ![t] = Done]
    /\ UNCHANGED <<now, readTs, alive, wrote, intent, ver, maxRead, recRead, reads, abortedFor, broken>>

\* The coordinator stops sending heartbeats: it died, or it is only late and may come back.
Stall(t) ==
    /\ coord[t] \in {Idle, Writing} /\ alive[t]
    /\ alive' = [alive EXCEPT ![t] = FALSE]
    /\ UNCHANGED <<now, coord, readTs, rec, wrote, intent, ver, maxRead, recRead, reads, abortedFor, broken>>

\* A late coordinator comes back; a dead one never does.
Resume(t) ==
    /\ ~alive[t] /\ coord[t] \in {Idle, Writing}
    /\ alive' = [alive EXCEPT ![t] = TRUE]
    /\ UNCHANGED <<now, coord, readTs, rec, wrote, intent, ver, maxRead, recRead, reads, abortedFor, broken>>

\* The reaper (or a writer's push) aborts a transaction without heartbeat; a missing record is
\* created Aborted.
Reap(t) ==
    /\ ~alive[t] /\ rec[t].st \in {None, Pending}
    /\ rec' = [rec EXCEPT ![t].st = Aborted]
    /\ UNCHANGED <<now, coord, readTs, alive, wrote, intent, ver, maxRead, recRead, reads, abortedFor, broken>>

\* ResolveIntents of a finished record (in the background or by a reader): the mutant turns an
\* intent into a version before the record is committed.
Resolve(t, k) ==
    /\ intent[k].t = t
    /\ \/ /\ rec[t].st = Committed
          /\ ver' = [ver EXCEPT ![k] = @ \cup {[t |-> t, ts |-> rec[t].cts]}]
       \/ /\ rec[t].st = Aborted
          /\ UNCHANGED ver
       \/ /\ Mutant = "resolve_before_commit" /\ rec[t].st = Pending
          /\ ver' = [ver EXCEPT ![k] = @ \cup {[t |-> t, ts |-> intent[k].ts]}]
    /\ intent' = [intent EXCEPT ![k] = NoIntent]
    /\ UNCHANGED <<now, coord, readTs, rec, alive, wrote, maxRead, recRead, reads, abortedFor, broken>>

\* What a push does to the record of t for a reader at rts: the record after it. Pushed by a read, a
\* live transaction's record stays as it is.
Pushed(t, rts) ==
    CASE ~alive[t] /\ rec[t].st \in {None, Pending} -> [rec[t] EXCEPT !.st = Aborted]
      [] PushByRead /\ rec[t].st \in {None, Pending} -> rec[t]
      [] rec[t].st = None -> [st |-> Pending, minC |-> rts + 1, cts |-> 0]
      [] rec[t].st = Pending -> [rec[t] EXCEPT !.minC = Max(@, rts + 1)]
      [] OTHER -> rec[t]

\* A read of key k at the current time.
Read(k) ==
    /\ Cardinality(reads) < MaxReads
    /\ LET rts == now
           owner == intent[k].t
           mustPush == owner # NoTxn /\ intent[k].ts <= rts
                       /\ rec[owner].st \in {None, Pending}
                       /\ Mutant # "reader_ignores_intent"
           after == IF mustPush THEN [rec EXCEPT ![owner] = Pushed(owner, rts)] ELSE rec
           byIntent == IF owner # NoTxn /\ intent[k].ts <= rts
                             /\ after[owner].st = Committed /\ after[owner].cts <= rts
                          THEN {owner} ELSE {}
           seen == {v.t : v \in {w \in ver[k] : w.ts <= rts}} \cup byIntent
       IN /\ rec' = after
          /\ abortedFor' = abortedFor \cup
                 (IF owner # NoTxn /\ after[owner].st = Aborted THEN {owner} ELSE {})
          /\ reads' = reads \cup {[k |-> k, rts |-> rts, seen |-> seen]}
          /\ maxRead' = [maxRead EXCEPT ![k] = Max(@, rts)]
          /\ recRead' = IF mustPush /\ PushByRead /\ alive[owner]
                           THEN [recRead EXCEPT ![owner] = Max(@, rts)] ELSE recRead
          \* A version written at or below the read after it was served breaks it too.
          /\ broken' = broken
    /\ UNCHANGED <<now, coord, readTs, alive, wrote, intent, ver>>

Next ==
    \/ Tick
    \/ \E t \in Txns : Begin(t) \/ CreateRecord(t) \/ Commit(t) \/ Abort(t) \/ Stall(t) \/ Resume(t)
          \/ Reap(t)
    \/ \E t \in Txns, k \in Keys : PutIntent(t, k) \/ Resolve(t, k)
    \/ \E k \in Keys : Read(k)

Spec == Init /\ [][Next]_vars

\* No read served at t misses a transaction that commits at or below t, and none commits after a
\* reader saw it aborted.
ReadsAreStable == ~broken

\* Only committed transactions leave versions, all at their commit timestamp.
VersionsAreCommitted ==
    \A k \in Keys : \A v \in ver[k] : rec[v.t].st = Committed /\ v.ts = rec[v.t].cts

\* First committer wins: two committed transactions that wrote the same key do not overlap.
FirstCommitterWins ==
    \A t1, t2 \in Txns :
        (t1 # t2 /\ rec[t1].st = Committed /\ rec[t2].st = Committed /\ wrote[t1] \cap wrote[t2] # {})
            => (rec[t1].cts <= readTs[t2] \/ rec[t2].cts <= readTs[t1])
=============================================================================
