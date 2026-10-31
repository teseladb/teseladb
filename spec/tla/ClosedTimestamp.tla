---------------------------- MODULE ClosedTimestamp ----------------------------
(* Closed timestamps and reads on followers (007 FR-012, D-013).                                  *)
(* The leader of a range closes a timestamp c: it will not write at c or below. It publishes c    *)
(* with the length of its log at that moment, in its heartbeats (which can be delayed, reordered  *)
(* or lost) and from time to time as a SetClosedTs entry of the log, so a restarted follower gets *)
(* it back. A write asks for a timestamp (a transaction's, which can be old) and the leader moves *)
(* it above its closed timestamp and to its lease start at least. A follower serves a read at t  *)
(* once it knows a closed c >= t whose log length it has applied. A new lease starts after the   *)
(* previous one ends and the old leader only closed timestamps within its lease, so the new      *)
(* leader, which only knows the closed timestamps of the log, never writes under one that a      *)
(* follower may have received in a heartbeat.                                                     *)
(* With Streams (014 FR-004, FR-019) time passes (now) and the leader closes a timestamp a fixed  *)
(* lag behind it: LagFar, or LagNear while change streams are open on the range, so the target    *)
(* moves forward and back as streams open and close. Transactions write intents (above the closed *)
(* timestamp, like a write) and commit them later, at the intent's timestamp or pushed above the  *)
(* closed one, or abort them. A follower read at t waits for the intents at or below t (it        *)
(* resolves them, 008 FR-013). A change stream emits the writes up to the resolved timestamp: the *)
(* lowest of the closed one and the timestamp just below the oldest pending intent.               *)
(* Invariant: a read on a follower, and a change stream up to its resolved timestamp, see exactly  *)
(* the writes at or below their timestamp that the range will ever have: no later write lands at  *)
(* or below the highest timestamp read or emitted (every later write goes after the entries they  *)
(* saw).                                                                                          *)
EXTENDS Naturals, Sequences, FiniteSets

CONSTANTS MaxTs, MaxLog, LeaseLen, MaxLeases, Streams, LagFar, LagNear

WriteE == "write"
ClosedE == "closed"
IntentE == "intent"
AbortedE == "aborted"

VARIABLES
    log,         \* the range's committed log: [k, ts] entries
    closed,      \* the current leader's closed timestamp
    leaseStart,  \* the current lease
    leaseEnd,
    leases,      \* leases granted so far
    inflight,    \* heartbeats on the way: <<closed, log length>>
    fApp,        \* the follower's applied entries
    fKnown,      \* closed timestamps the follower knows: <<closed, log length>>
    maxRead,     \* ghost: the highest timestamp a follower read was served at
    broken,      \* ghost: a follower read or a change stream missed a write at or below its timestamp
    now,         \* time (with Streams)
    open,        \* change streams are open on the range
    pend,        \* log indices of the pending intents
    emitted      \* ghost: the highest resolved timestamp a change stream emitted up to

vars == <<log, closed, leaseStart, leaseEnd, leases, inflight, fApp, fKnown, maxRead, broken, now, open, pend,
          emitted>>

Init ==
    /\ log = <<>> /\ closed = 0
    /\ leaseStart = 1 /\ leaseEnd = LeaseLen /\ leases = 1
    /\ inflight = {} /\ fApp = 0 /\ fKnown = {} /\ maxRead = 0 /\ broken = FALSE
    /\ now = 0 /\ open = FALSE /\ pend = {} /\ emitted = 0

Max(a, b) == IF a > b THEN a ELSE b
Min(a, b) == IF a < b THEN a ELSE b

\* The indices of the writes at or below t among the first n entries.
Seen(n, t) == {i \in 1..n : log[i].k = WriteE /\ log[i].ts <= t}

\* A write at ts lands under what a follower read or a change stream already saw.
Breaks(ts) == broken \/ ts <= maxRead \/ ts <= emitted

\* The timestamp a write asks for, moved above the closed one and to the lease start.
WriteTs(r) == Max(Max(r, closed + 1), leaseStart)

Write(r) ==
    LET ts == WriteTs(r) IN
    /\ Len(log) < MaxLog /\ ts <= MaxTs
    /\ log' = Append(log, [k |-> WriteE, ts |-> ts])
    /\ broken' = Breaks(ts)
    /\ UNCHANGED <<closed, leaseStart, leaseEnd, leases, inflight, fApp, fKnown, maxRead, now, open, pend, emitted>>

\* The lag of the closed timestamp behind the time: shorter while change streams are open.
Lag == IF open THEN LagNear ELSE LagFar

\* The leader closes c within its lease (with Streams, at most its lag behind the time) and sends it
\* in a heartbeat with its log length.
Close(c) ==
    /\ c > closed /\ c <= leaseEnd
    /\ ~Streams \/ c + Lag <= now
    /\ closed' = c
    /\ inflight' = inflight \cup {<<c, Len(log)>>}
    /\ UNCHANGED <<log, leaseStart, leaseEnd, leases, fApp, fKnown, maxRead, broken, now, open, pend, emitted>>

\* SetClosedTs: the closed timestamp goes into the log, valid from its own index.
LogClosed ==
    /\ Len(log) < MaxLog /\ closed > 0
    /\ log' = Append(log, [k |-> ClosedE, ts |-> closed])
    /\ UNCHANGED <<closed, leaseStart, leaseEnd, leases, inflight, fApp, fKnown, maxRead, broken, now, open, pend,
                   emitted>>

Deliver(m) ==
    /\ fKnown' = fKnown \cup {m}
    /\ inflight' = inflight \ {m}
    /\ UNCHANGED <<log, closed, leaseStart, leaseEnd, leases, fApp, maxRead, broken, now, open, pend, emitted>>

Lose(m) ==
    /\ inflight' = inflight \ {m}
    /\ UNCHANGED <<log, closed, leaseStart, leaseEnd, leases, fApp, fKnown, maxRead, broken, now, open, pend, emitted>>

FollowerApply ==
    /\ fApp < Len(log)
    /\ fApp' = fApp + 1
    /\ fKnown' = IF log[fApp + 1].k = ClosedE
                 THEN fKnown \cup {<<log[fApp + 1].ts, fApp + 1>>} ELSE fKnown
    /\ UNCHANGED <<log, closed, leaseStart, leaseEnd, leases, inflight, maxRead, broken, now, open, pend, emitted>>

\* A follower restart forgets the heartbeats; the log brings back what was logged.
FollowerRestart ==
    /\ fApp' = 0 /\ fKnown' = {}
    /\ UNCHANGED <<log, closed, leaseStart, leaseEnd, leases, inflight, maxRead, broken, now, open, pend, emitted>>

\* It resolves the intents at or below t first, so it reads once none is pending there.
FollowerRead(t) ==
    /\ \E m \in fKnown : m[2] <= fApp /\ t <= m[1]
    /\ \A i \in pend : log[i].ts > t
    /\ maxRead' = Max(maxRead, t)
    /\ broken' = (broken \/ Seen(fApp, t) /= Seen(Len(log), t))
    /\ UNCHANGED <<log, closed, leaseStart, leaseEnd, leases, inflight, fApp, fKnown, now, open, pend, emitted>>

\* A new leader: its lease starts after the previous one ends; it knows the logged closed ones.
NewLease ==
    /\ leases < MaxLeases
    /\ leases' = leases + 1
    /\ leaseStart' = leaseEnd + 1
    /\ leaseEnd' = leaseEnd + LeaseLen
    /\ closed' = LET logged == {log[i].ts : i \in {j \in 1..Len(log) : log[j].k = ClosedE}}
                 IN IF logged = {} THEN 0 ELSE CHOOSE c \in logged : \A d \in logged : d <= c
    /\ UNCHANGED <<log, inflight, fApp, fKnown, maxRead, broken, now, open, pend, emitted>>

Tick ==
    /\ Streams /\ now < MaxTs
    /\ now' = now + 1
    /\ UNCHANGED <<log, closed, leaseStart, leaseEnd, leases, inflight, fApp, fKnown, maxRead, broken, open, pend,
                   emitted>>

\* A change stream opens on the range, or the last one closes: the closed timestamp's target moves.
Toggle ==
    /\ Streams
    /\ open' = ~open
    /\ UNCHANGED <<log, closed, leaseStart, leaseEnd, leases, inflight, fApp, fKnown, maxRead, broken, now, pend,
                   emitted>>

\* A transaction writes an intent, above the closed timestamp like a write.
Intent(r) ==
    LET ts == WriteTs(r) IN
    /\ Streams /\ Len(log) < MaxLog /\ ts <= MaxTs
    /\ log' = Append(log, [k |-> IntentE, ts |-> ts])
    /\ pend' = pend \cup {Len(log) + 1}
    /\ UNCHANGED <<closed, leaseStart, leaseEnd, leases, inflight, fApp, fKnown, maxRead, broken, now, open, emitted>>

\* The transaction commits at the intent's timestamp: the intent at index i becomes its version, which
\* a reader that applied i sees once it resolves the intent. Or it commits pushed, at a timestamp the
\* leader may write at (above its closed timestamp and from its lease start): the intent goes and the
\* version is written at c.
Commit(i, c) ==
    LET p == log[i].ts IN
    /\ Streams /\ i \in pend
    /\ \/ /\ c = p
          /\ log' = [log EXCEPT ![i].k = WriteE]
       \/ /\ c > p /\ c = WriteTs(c) /\ Len(log) < MaxLog
          /\ log' = Append([log EXCEPT ![i].k = AbortedE], [k |-> WriteE, ts |-> c])
    /\ pend' = pend \ {i}
    /\ broken' = Breaks(c)
    /\ UNCHANGED <<closed, leaseStart, leaseEnd, leases, inflight, fApp, fKnown, maxRead, now, open, emitted>>

Abort(i) ==
    /\ Streams /\ i \in pend
    /\ log' = [log EXCEPT ![i].k = AbortedE]
    /\ pend' = pend \ {i}
    /\ UNCHANGED <<closed, leaseStart, leaseEnd, leases, inflight, fApp, fKnown, maxRead, broken, now, open,
                   emitted>>

\* The resolved timestamp of the range (014 FR-004).
Resolved == IF pend = {} THEN closed
            ELSE Min(closed, (CHOOSE p \in {log[i].ts : i \in pend} : \A j \in pend : p <= log[j].ts) - 1)

\* A change stream on the leader, which has the whole log, emits every write up to the resolved
\* timestamp; a write that lands at or below it later is one the stream lost (Breaks).
Emit ==
    /\ Streams /\ open
    /\ emitted' = Max(emitted, Resolved)
    /\ UNCHANGED <<log, closed, leaseStart, leaseEnd, leases, inflight, fApp, fKnown, maxRead, broken, now, open,
                   pend>>

Next ==
    \/ \E r \in 1..MaxTs : Write(r) \/ Intent(r)
    \/ \E c \in 1..MaxTs : Close(c)
    \/ LogClosed \/ FollowerApply \/ FollowerRestart \/ NewLease \/ Tick \/ Toggle \/ Emit
    \/ \E m \in inflight : Deliver(m) \/ Lose(m)
    \/ \E t \in 1..MaxTs : FollowerRead(t)
    \/ \E i \in pend : Abort(i) \/ \E c \in 1..MaxTs : Commit(i, c)

Spec == Init /\ [][Next]_vars

TypeOK == closed \in 0..MaxTs /\ fApp \in 0..MaxLog /\ now \in 0..MaxTs /\ emitted \in 0..MaxTs

FollowerReadsStable == ~broken
=============================================================================
