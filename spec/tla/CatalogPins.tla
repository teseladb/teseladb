------------------------------- MODULE CatalogPins -------------------------------
(* The pins of the catalog reads of a node's connection threads, without a message to core 0     *)
(* (009 FR-006, H-013 of the native tests on YCSB-E). Core 0 publishes the catalog of the indexes *)
(* at a Group 0 index; a thread that starts an operation reads the published index, writes it in  *)
(* its own pin slot and reads the publication again: if it moved, it starts over with the new one. *)
(* Core 0 acknowledges a new index to the fence of an index change only after it published the    *)
(* catalog at that index and found no slot below it. With sequentially consistent atomics either  *)
(* core 0 sees the thread's pin, or the thread sees the new publication and starts over.           *)
(* RecheckAfterPin and PublishBeforeScan turn each step off: without either NoOlderAfterAck fails  *)
(* (09-11-2026, depth 9).                                                                          *)
(* Invariant: once core 0 acknowledged index F, no running operation works with a catalog older    *)
(* than F (the barrier of OnlineIndexBuild.tla).                                                   *)
EXTENDS Naturals, FiniteSets

CONSTANTS Threads, MaxIndex, RecheckAfterPin, PublishBeforeScan

VARIABLES
    published,  \* the Group 0 index of the catalog core 0 published
    latest,     \* the newest index of the catalog in Group 0
    acked,      \* the newest index core 0 acknowledged to the fence
    slot,       \* each thread's pin slot (0: none)
    read,       \* the index a thread read before writing its slot (0: none)
    using,      \* the catalog index each running operation works with (0: none)
    scanned,    \* core 0's pending acknowledgement: the index it is about to acknowledge (0: none)
    seen        \* the slots core 0 read for it

vars == <<published, latest, acked, slot, read, using, scanned, seen>>

Min(s) == IF s = {} THEN MaxIndex + 1 ELSE CHOOSE m \in s : \A n \in s : m <= n

Init ==
    /\ published = 1
    /\ latest = 1
    /\ acked = 1
    /\ slot = [t \in Threads |-> 0]
    /\ read = [t \in Threads |-> 0]
    /\ using = [t \in Threads |-> 0]
    /\ scanned = 0
    /\ seen = {}

\* Group 0 applies an index change: a new catalog exists.
Change ==
    /\ latest < MaxIndex
    /\ latest' = latest + 1
    /\ UNCHANGED <<published, acked, slot, read, using, scanned, seen>>

\* A thread with no operation reads the published index.
ReadPublished(t) ==
    /\ using[t] = 0 /\ read[t] = 0
    /\ read' = [read EXCEPT ![t] = published]
    /\ UNCHANGED <<published, latest, acked, slot, using, scanned, seen>>

\* ... writes it in its slot and reads the publication again: the same, the operation runs with
\* it; moved, it starts over. Without the second read it always runs.
Pin(t) ==
    /\ read[t] # 0
    /\ slot' = [slot EXCEPT ![t] = read[t]]
    /\ IF ~RecheckAfterPin \/ published = read[t]
          THEN /\ using' = [using EXCEPT ![t] = read[t]]
               /\ read' = [read EXCEPT ![t] = 0]
          ELSE /\ read' = [read EXCEPT ![t] = 0]
               /\ UNCHANGED using
    /\ UNCHANGED <<published, latest, acked, scanned, seen>>

\* The operation ends and its slot clears.
End(t) ==
    /\ using[t] # 0
    /\ using' = [using EXCEPT ![t] = 0]
    /\ slot' = [slot EXCEPT ![t] = 0]
    /\ UNCHANGED <<published, latest, acked, read, scanned, seen>>

\* Core 0 publishes the newest catalog and then reads every slot, to acknowledge it; in the
\* faulty variant it reads the slots first and publishes after.
Publish ==
    /\ scanned = 0 /\ acked < latest
    /\ IF PublishBeforeScan
          THEN /\ published' = latest
               /\ seen' = {slot[t] : t \in Threads} \ {0}
          ELSE /\ seen' = {slot[t] : t \in Threads} \ {0}
               /\ UNCHANGED published
    /\ scanned' = latest
    /\ UNCHANGED <<latest, acked, slot, read, using>>

\* ... and acknowledges it if no slot it read was below it; otherwise it scans again later.
Ack ==
    /\ scanned # 0
    /\ published' = scanned
    /\ IF Min(seen) >= scanned THEN acked' = scanned ELSE UNCHANGED acked
    /\ scanned' = 0
    /\ seen' = {}
    /\ UNCHANGED <<latest, slot, read, using>>

Next == Change \/ Publish \/ Ack \/ \E t \in Threads : ReadPublished(t) \/ Pin(t) \/ End(t)

Spec == Init /\ [][Next]_vars

\* Once core 0 acknowledged index F, no running operation works with a catalog older than F.
NoOlderAfterAck == \A t \in Threads : using[t] = 0 \/ using[t] >= acked
================================================================================
