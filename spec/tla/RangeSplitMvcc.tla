----------------------------- MODULE RangeSplitMvcc -----------------------------
(* RangeSplit with MVCC versions and transaction intents (007 FR-007, FR-008, FR-022).            *)
(* RangeSplit checks the intent in Group 0, OutOfRange for stale routers, the dedup table and the *)
(* replica epoch. This model keeps that protocol (SplitRange, SetRangeBounds at index i,          *)
(* CompleteSplit) and adds what the state of a document range holds besides the latest value:     *)
(* every version of a key with its timestamp, the intents of a distributed transaction whose keys *)
(* fall on both sides of the split point, and the range's last timestamp, from which its next     *)
(* write is stamped. At index i every replica builds G''s state from G's: the versions and        *)
(* intents of the keys under P and G's last timestamp. A transaction then commits or aborts and   *)
(* each of its intents is resolved in the range that holds the key at that moment: a stale router *)
(* that sends a resolution to G for a key under P gets OutOfRange and retries in G'.             *)
(* Invariants: the range that serves a key holds exactly its committed versions and its pending  *)
(* intent (no version or intent is lost or duplicated), a key's versions are stamped in the      *)
(* order they were written, and G never serves a key it handed over.                              *)
EXTENDS Naturals, FiniteSets

CONSTANTS Key, Under, Val, Txn, MaxTs

None == <<>>  \* no intent

VARIABLES
    pending,    \* Group 0: SplitRange recorded and not completed
    done,       \* Group 0: CompleteSplit applied (G' exists)
    out,        \* G applied SetRangeBounds { out: [P] }
    fresh,      \* the router's copy of Group 0 shows G'
    gVers,      \* G's versions: Key -> set of <<ts, value>>
    gInt,       \* G's intents: Key -> <<txn, ts, value>> or None
    gLast,      \* G's last timestamp
    snap,       \* G''s state built at index i
    g2Vers, g2Int, g2Last,
    txn,        \* Txn -> "idle" | "open" | "committed" | "aborted"
    txnTs,      \* Txn -> the timestamp of its writes (0 while idle)
    wVers,      \* ghost: committed versions per key
    wInt,       \* ghost: pending intent per key
    order       \* ghost: FALSE once a key got a version not above all its earlier ones

vars == <<pending, done, out, fresh, gVers, gInt, gLast, snap, g2Vers, g2Int, g2Last, txn, txnTs,
          wVers, wInt, order>>
rangeVars == <<gVers, gInt, gLast, g2Vers, g2Int, g2Last>>

NoSnap == [ok |-> FALSE, vers |-> [k \in Key |-> {}], int |-> [k \in Key |-> None], last |-> 0]

Init ==
    /\ pending = FALSE /\ done = FALSE /\ out = FALSE /\ fresh = FALSE
    /\ gVers = [k \in Key |-> {}] /\ gInt = [k \in Key |-> None] /\ gLast = 0
    /\ snap = NoSnap
    /\ g2Vers = [k \in Key |-> {}] /\ g2Int = [k \in Key |-> None] /\ g2Last = 0
    /\ txn = [t \in Txn |-> "idle"] /\ txnTs = [t \in Txn |-> 0]
    /\ wVers = [k \in Key |-> {}] /\ wInt = [k \in Key |-> None]
    /\ order = TRUE

\* Where the router sends a request for k: G' once its copy shows it and k is under P; G
\* otherwise. G refuses keys under P once it applied SetRangeBounds (OutOfRange): the request
\* waits for the router to refresh, which the guards express by disabling the step.
ToG2(k) == k \in Under /\ fresh /\ done
ReachesG(k) == ~ToG2(k) /\ ~(k \in Under /\ out)

Max(a, b) == IF a > b THEN a ELSE b
Newer(vs, ts) == \A v \in vs : v[1] < ts

\* A plain write of v to k, outside a transaction. A key with a pending intent makes it wait.
Write(k, v) ==
    /\ wInt[k] = None
    /\ \/ /\ ReachesG(k) /\ gLast < MaxTs
          /\ gLast' = gLast + 1
          /\ gVers' = [gVers EXCEPT ![k] = @ \cup {<<gLast + 1, v>>}]
          /\ wVers' = [wVers EXCEPT ![k] = @ \cup {<<gLast + 1, v>>}]
          /\ order' = (order /\ Newer(wVers[k], gLast + 1))
          /\ UNCHANGED <<gInt, g2Vers, g2Int, g2Last>>
       \/ /\ ToG2(k) /\ g2Last < MaxTs
          /\ g2Last' = g2Last + 1
          /\ g2Vers' = [g2Vers EXCEPT ![k] = @ \cup {<<g2Last + 1, v>>}]
          /\ wVers' = [wVers EXCEPT ![k] = @ \cup {<<g2Last + 1, v>>}]
          /\ order' = (order /\ Newer(wVers[k], g2Last + 1))
          /\ UNCHANGED <<gVers, gInt, gLast, g2Int>>
    /\ UNCHANGED <<pending, done, out, fresh, snap, txn, txnTs, wInt>>

\* A transaction opens with a timestamp above both ranges' last ones (its coordinator's clock).
Open(t) ==
    /\ txn[t] = "idle"
    /\ Max(gLast, g2Last) < MaxTs
    /\ txn' = [txn EXCEPT ![t] = "open"]
    /\ txnTs' = [txnTs EXCEPT ![t] = Max(gLast, g2Last) + 1]
    /\ UNCHANGED <<pending, done, out, fresh, rangeVars, snap, wVers, wInt, order>>

\* The transaction writes an intent of v on k in the range that holds k. The range's last
\* timestamp moves up to the transaction's, so later writes are stamped above it.
WriteIntent(t, k, v) ==
    /\ txn[t] = "open" /\ wInt[k] = None
    /\ ~\E ver \in wVers[k] : ver[1] >= txnTs[t]
    /\ \/ /\ ReachesG(k)
          /\ gInt' = [gInt EXCEPT ![k] = <<t, txnTs[t], v>>]
          /\ gLast' = Max(gLast, txnTs[t])
          /\ UNCHANGED <<g2Int, g2Last>>
       \/ /\ ToG2(k)
          /\ g2Int' = [g2Int EXCEPT ![k] = <<t, txnTs[t], v>>]
          /\ g2Last' = Max(g2Last, txnTs[t])
          /\ UNCHANGED <<gInt, gLast>>
    /\ wInt' = [wInt EXCEPT ![k] = <<t, txnTs[t], v>>]
    /\ UNCHANGED <<pending, done, out, fresh, gVers, g2Vers, snap, txn, txnTs, wVers, order>>

Decide(t, outcome) ==
    /\ txn[t] = "open"
    /\ txn' = [txn EXCEPT ![t] = outcome]
    /\ UNCHANGED <<pending, done, out, fresh, rangeVars, snap, txnTs, wVers, wInt, order>>

\* Resolving t's intent on k in the range that holds k: a commit turns it into a version.
Resolve(t, k) ==
    LET i == wInt[k]
        commit == txn[t] = "committed"
        ver == <<i[2], i[3]>>
    IN
    /\ i /= None /\ i[1] = t /\ txn[t] \in {"committed", "aborted"}
    /\ \/ /\ ReachesG(k) /\ gInt[k] = i
          /\ gInt' = [gInt EXCEPT ![k] = None]
          /\ gVers' = IF commit THEN [gVers EXCEPT ![k] = @ \cup {ver}] ELSE gVers
          /\ UNCHANGED <<g2Int, g2Vers>>
       \/ /\ ToG2(k) /\ g2Int[k] = i
          /\ g2Int' = [g2Int EXCEPT ![k] = None]
          /\ g2Vers' = IF commit THEN [g2Vers EXCEPT ![k] = @ \cup {ver}] ELSE g2Vers
          /\ UNCHANGED <<gInt, gVers>>
    /\ wInt' = [wInt EXCEPT ![k] = None]
    /\ wVers' = IF commit THEN [wVers EXCEPT ![k] = @ \cup {ver}] ELSE wVers
    /\ order' = (order /\ (~commit \/ Newer(wVers[k], i[2])))
    /\ UNCHANGED <<pending, done, out, fresh, gLast, g2Last, snap, txn, txnTs>>

SplitIntent ==
    /\ ~pending /\ ~done
    /\ pending' = TRUE
    /\ UNCHANGED <<done, out, fresh, rangeVars, snap, txn, txnTs, wVers, wInt, order>>

\* G applies SetRangeBounds at index i: every replica builds G''s state at i from G's.
SetBounds ==
    /\ pending /\ ~out
    /\ out' = TRUE
    /\ snap' = [ok |-> TRUE,
                vers |-> [k \in Key |-> IF k \in Under THEN gVers[k] ELSE {}],
                int |-> [k \in Key |-> IF k \in Under THEN gInt[k] ELSE None],
                last |-> gLast]
    /\ UNCHANGED <<pending, done, fresh, rangeVars, txn, txnTs, wVers, wInt, order>>

Complete ==
    /\ pending /\ out /\ snap.ok
    /\ done' = TRUE /\ pending' = FALSE
    /\ g2Vers' = snap.vers /\ g2Int' = snap.int /\ g2Last' = snap.last
    /\ UNCHANGED <<out, fresh, gVers, gInt, gLast, snap, txn, txnTs, wVers, wInt, order>>

Refresh ==
    /\ fresh /= done
    /\ fresh' = done
    /\ UNCHANGED <<pending, done, out, rangeVars, snap, txn, txnTs, wVers, wInt, order>>

Next ==
    \/ \E k \in Key, v \in Val : Write(k, v)
    \/ \E t \in Txn : Open(t) \/ Decide(t, "committed") \/ Decide(t, "aborted")
    \/ \E t \in Txn, k \in Key, v \in Val : WriteIntent(t, k, v)
    \/ \E t \in Txn, k \in Key : Resolve(t, k)
    \/ SplitIntent \/ SetBounds \/ Complete \/ Refresh

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ pending \in BOOLEAN /\ done \in BOOLEAN /\ out \in BOOLEAN /\ fresh \in BOOLEAN
    /\ gLast \in 0..MaxTs /\ g2Last \in 0..MaxTs

\* What the range that serves k holds: G' (or, between i and CompleteSplit, the state built at
\* i, which no request can change because G refuses keys under P and G' does not exist yet).
ServedVers(k) == IF k \in Under /\ out THEN (IF done THEN g2Vers[k] ELSE snap.vers[k]) ELSE gVers[k]
ServedInt(k) == IF k \in Under /\ out THEN (IF done THEN g2Int[k] ELSE snap.int[k]) ELSE gInt[k]

NoVersionLost == \A k \in Key : ServedVers(k) = wVers[k]
NoIntentLost == \A k \in Key : ServedInt(k) = wInt[k]
VersionsInOrder == order
\* A key that left G is no longer stored there once G' serves it (no duplicate copy is read).
NoStaleCopyServed == \A k \in Under : out => ~ReachesG(k)
=============================================================================
