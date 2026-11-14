---------------------------- MODULE StatementBarrier ----------------------------
(* A command of several write statements that waits for its write concern once (005 FR-036).     *)
(* Each statement is acknowledged to the command once its entry is in the leader's log, with the   *)
(* term and the index of the entry. When the command has written its statements it asks the leader *)
(* of the range for a barrier with the last entry of each term it wrote in. The leader answers     *)
(* once it holds each of them, with its term, below the commit index it knows, and the command     *)
(* answers its client then. Other clients write to the same range meanwhile.                       *)
(* Invariant: every statement of an answered command is committed.                                 *)
EXTENDS Naturals, Sequences, FiniteSets

CONSTANTS
    Node,        \* the replicas of the range
    MaxTerm,
    Statements,  \* statements of the command
    Others,      \* writes of other clients
    ChecksTerm,  \* the barrier compares the term of each entry (FALSE: its index alone)
    EveryTerm    \* the barrier carries the last entry of each term (FALSE: the last entry alone)

VARIABLES
    term,       \* each node's term
    leader,     \* each node believes it leads its term
    log,        \* each node's log: entries [t |-> term, s |-> statement or other write]
    known,      \* commit index each leader knows
    committed,  \* entries committed so far
    sent,       \* the entry of each statement written so far: [t |-> term, i |-> index]
    others,     \* writes of other clients so far
    answered    \* the command answered its client

vars == <<term, leader, log, known, committed, sent, others, answered>>

Majority(Q) == 2 * Cardinality(Q) > Cardinality(Node)
Last(s) == IF s = <<>> THEN 0 ELSE s[Len(s)].t
UpToDate(c, n) == Last(log[c]) > Last(log[n]) \/ (Last(log[c]) = Last(log[n]) /\ Len(log[c]) >= Len(log[n]))
Entries(s, a, b) == {s[k] : k \in a..b}

Init ==
    /\ term = [n \in Node |-> 0]
    /\ leader = [n \in Node |-> FALSE]
    /\ log = [n \in Node |-> <<>>]
    /\ known = [n \in Node |-> 0]
    /\ committed = {}
    /\ sent = <<>>
    /\ others = 0
    /\ answered = FALSE

\* The command's next statement: in the leader's log, it is acknowledged with its term and index.
Statement(l) ==
    /\ leader[l] /\ ~answered /\ Len(sent) < Statements
    /\ log' = [log EXCEPT ![l] = Append(@, [t |-> term[l], s |-> Len(sent) + 1])]
    /\ sent' = Append(sent, [t |-> term[l], i |-> Len(log[l]) + 1])
    /\ UNCHANGED <<term, leader, known, committed, others, answered>>

\* A write of another client.
Other(l) ==
    /\ leader[l] /\ others < Others
    /\ log' = [log EXCEPT ![l] = Append(@, [t |-> term[l], s |-> Statements + others + 1])]
    /\ others' = others + 1
    /\ UNCHANGED <<term, leader, known, committed, sent, answered>>

\* AppendEntries carrying the leader's whole log.
Replicate(l, f) ==
    /\ leader[l] /\ f /= l /\ term[f] <= term[l]
    /\ log' = [log EXCEPT ![f] = log[l]]
    /\ term' = [term EXCEPT ![f] = term[l]]
    /\ leader' = [leader EXCEPT ![f] = FALSE]
    /\ UNCHANGED <<known, committed, sent, others, answered>>

\* The leader commits up to k: an entry of its term in the log of a quorum of its term.
Commit(l, k, q) ==
    /\ leader[l] /\ l \in q /\ Majority(q)
    /\ k \in (known[l] + 1)..Len(log[l])
    /\ log[l][k].t = term[l]
    /\ \A n \in q : term[n] = term[l] /\ Len(log[n]) >= k /\ log[n][k] = log[l][k]
    /\ known' = [known EXCEPT ![l] = k]
    /\ committed' = committed \cup Entries(log[l], 1, k)
    /\ UNCHANGED <<term, leader, log, sent, others, answered>>

\* Candidate c wins term t with the votes of quorum q, whose logs are not newer than its own.
Elect(c, q) ==
    LET t == term[c] + 1 IN
    /\ t <= MaxTerm /\ c \in q /\ Majority(q)
    /\ \A n \in q : term[n] < t /\ UpToDate(c, n)
    /\ term' = [n \in Node |-> IF n \in q THEN t ELSE term[n]]
    /\ leader' = [n \in Node |-> IF n = c THEN TRUE ELSE IF n \in q THEN FALSE ELSE leader[n]]
    /\ known' = [known EXCEPT ![c] = 0]
    /\ UNCHANGED <<log, committed, sent, others, answered>>

\* The entries the barrier carries: the last one of each term the command wrote in.
Carried ==
    IF EveryTerm
    THEN {sent[k] : k \in {j \in 1..Len(sent) : \A m \in (j + 1)..Len(sent) : sent[m].t /= sent[j].t}}
    ELSE {sent[Len(sent)]}

\* The leader holds entry p, with its term, below the commit index it knows.
Holds(l, p) ==
    /\ p.i <= Len(log[l])
    /\ ChecksTerm => log[l][p.i].t = p.t
    /\ known[l] >= p.i

\* The barrier: the leader answers it and the command answers its client.
Barrier(l) ==
    /\ leader[l] /\ ~answered /\ Len(sent) = Statements
    /\ \A p \in Carried : Holds(l, p)
    /\ answered' = TRUE
    /\ UNCHANGED <<term, leader, log, known, committed, sent, others>>

Next ==
    \/ \E l \in Node : Statement(l) \/ Other(l) \/ Barrier(l)
    \/ \E l, f \in Node : Replicate(l, f)
    \/ \E l \in Node, k \in 1..(Statements + Others), q \in SUBSET Node : Commit(l, k, q)
    \/ \E c \in Node, q \in SUBSET Node : Elect(c, q)

Spec == Init /\ [][Next]_vars

AnsweredIsCommitted ==
    answered => \A k \in 1..Len(sent) : [t |-> sent[k].t, s |-> k] \in committed

TypeOK ==
    /\ term \in [Node -> 0..MaxTerm]
    /\ Len(sent) <= Statements /\ others <= Others
    /\ \A n \in Node : Len(log[n]) <= Statements + Others
=============================================================================
