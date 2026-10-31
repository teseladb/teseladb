------------------------------ MODULE Meta2Handoff ------------------------------
(* Handing the range descriptors over from Group 0 to Meta2 ranges (007 FR-014, D-012).           *)
(* Before the handoff Group 0 holds every descriptor. The handoff is one Group 0 entry at index i:*)
(* from i Group 0 keeps Meta1 (which Meta2 range holds which descriptors) and stops answering     *)
(* lookups and accepting updates of descriptors; each Meta2 range is created with the descriptors *)
(* it covers as they were at i, the way a split builds the new range's state. A descriptor update *)
(* (a split or a replica change of a data range, conditional on the generation it read) goes to  *)
(* whichever holder its sender believes in: Group 0 after i answers with Meta1 and the sender     *)
(* retries at the Meta2 range; a Meta2 range that does not exist yet sends it back to Group 0.    *)
(* Routers look descriptors up the same way and cache them. Every Meta2 range is created by the   *)
(* same entry, so the model keeps their descriptors in one map: which Meta2 range holds which     *)
(* descriptor does not change the protocol.                                                      *)
(* Invariants: every acknowledged update is in the holder's descriptor (none lost), exactly one   *)
(* holder answers for each descriptor (none duplicated) and a lookup never answers a generation   *)
(* older than one already acknowledged.                                                          *)
EXTENDS Naturals, FiniteSets

CONSTANTS Range, MaxGen

G0 == "group0"
M2 == "meta2"

VARIABLES
    handed,     \* Group 0 applied the handoff
    g0Gen,      \* Group 0's descriptors: Range -> generation
    m2Gen,      \* the Meta2 ranges' descriptors: Range -> generation (0: the Meta2 range is empty)
    believe,    \* Range -> the holder the range's leader sends its updates to
    rBelieve,   \* Range -> the holder the router asks
    cache,      \* the router's cache: Range -> generation
    acked,      \* ghost: acknowledged updates per range
    staleLookup \* ghost: a lookup answered a generation older than an acknowledged one

vars == <<handed, g0Gen, m2Gen, believe, rBelieve, cache, acked, staleLookup>>

Init ==
    /\ handed = FALSE
    /\ g0Gen = [r \in Range |-> 1] /\ m2Gen = [r \in Range |-> 0]
    /\ believe = [r \in Range |-> G0] /\ rBelieve = [r \in Range |-> G0]
    /\ cache = [r \in Range |-> 1]
    /\ acked = [r \in Range |-> 1]
    /\ staleLookup = FALSE

Handoff ==
    /\ ~handed
    /\ handed' = TRUE
    /\ m2Gen' = [r \in Range |-> g0Gen[r]]
    /\ UNCHANGED <<g0Gen, believe, rBelieve, cache, acked, staleLookup>>

\* Group 0 answers updates and lookups only before the handoff.
G0Serves == ~handed
M2Serves == handed

\* The range's leader updates its descriptor from generation acked[r] at the holder it believes.
Update(r) ==
    /\ acked[r] < MaxGen
    /\ \/ /\ believe[r] = G0 /\ G0Serves /\ g0Gen[r] = acked[r]
          /\ g0Gen' = [g0Gen EXCEPT ![r] = @ + 1]
          /\ acked' = [acked EXCEPT ![r] = @ + 1]
          /\ UNCHANGED <<m2Gen, believe>>
       \/ /\ believe[r] = M2 /\ M2Serves /\ m2Gen[r] = acked[r]
          /\ m2Gen' = [m2Gen EXCEPT ![r] = @ + 1]
          /\ acked' = [acked EXCEPT ![r] = @ + 1]
          /\ UNCHANGED <<g0Gen, believe>>
       \* Refused: Group 0 after the handoff answers with Meta1, an empty Meta2 range with Group 0.
       \/ /\ believe[r] = G0 /\ ~G0Serves
          /\ believe' = [believe EXCEPT ![r] = M2]
          /\ UNCHANGED <<g0Gen, m2Gen, acked>>
       \/ /\ believe[r] = M2 /\ ~M2Serves
          /\ believe' = [believe EXCEPT ![r] = G0]
          /\ UNCHANGED <<g0Gen, m2Gen, acked>>
    /\ UNCHANGED <<handed, rBelieve, cache, staleLookup>>

\* The router looks r up at the holder it believes in (after an OutOfRange from the data range,
\* or to fill its cache) and caches the answer; a refusal sends it to the other holder.
Lookup(r) ==
    \/ /\ rBelieve[r] = G0 /\ G0Serves
       /\ cache' = [cache EXCEPT ![r] = g0Gen[r]]
       /\ staleLookup' = (staleLookup \/ g0Gen[r] < acked[r])
       /\ UNCHANGED rBelieve
    \/ /\ rBelieve[r] = M2 /\ M2Serves
       /\ cache' = [cache EXCEPT ![r] = m2Gen[r]]
       /\ staleLookup' = (staleLookup \/ m2Gen[r] < acked[r])
       /\ UNCHANGED rBelieve
    \/ /\ rBelieve[r] = G0 /\ ~G0Serves
       /\ rBelieve' = [rBelieve EXCEPT ![r] = M2]
       /\ UNCHANGED <<cache, staleLookup>>
    \/ /\ rBelieve[r] = M2 /\ ~M2Serves
       /\ rBelieve' = [rBelieve EXCEPT ![r] = G0]
       /\ UNCHANGED <<cache, staleLookup>>

LookupStep(r) == Lookup(r) /\ UNCHANGED <<handed, g0Gen, m2Gen, believe, acked>>

Next == Handoff \/ \E r \in Range : Update(r) \/ LookupStep(r)

Spec == Init /\ [][Next]_vars

TypeOK == handed \in BOOLEAN /\ \A r \in Range : acked[r] \in 1..MaxGen

Holder(r) == IF handed THEN m2Gen[r] ELSE g0Gen[r]
NoLostUpdate == \A r \in Range : Holder(r) = acked[r]
OneHolder == G0Serves # M2Serves
NoStaleLookup == ~staleLookup
\* Every descriptor reaches its Meta2 range at the handoff.
HandoffComplete == handed => \A r \in Range : m2Gen[r] > 0
=============================================================================
