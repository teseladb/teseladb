# Licensing TeselaDB

TeselaDB is distributed under the [Business Source License 1.1](LICENSE) (BUSL). This page explains,
in plain words, who can use it for free and who needs a commercial license. If this page and
[LICENSE](LICENSE) ever disagree, LICENSE is what applies.

## Free for you if

- you only develop, test, evaluate, study or modify TeselaDB, without running it in production; or
- you run it in production, its nodes use 64 GB of RAM or less in total, and either your use is
  non-commercial (personal projects, education, academic research, a non-profit organization or a
  public administration) or your organization, together with every company that controls it, is
  controlled by it or shares its owner, had less than 1,000,000 euros of revenue in its last
  completed fiscal year.

In every case you must not offer TeselaDB to third parties as a hosted or managed service, such as
selling a database service built on it. Running TeselaDB as the database of your own applications,
even when your customers use those applications, is not a hosted service.

## How the RAM is counted

The RAM of a node is the memory its process can use: the limit of its container or control group
(`--memory` of Docker, `limits.memory` of Kubernetes, `memory.max` of cgroup v2), or the total memory
of the machine when there is no limit, rounded up to the whole gigabyte. The total is the sum over
every node of TeselaDB that your organization runs in production at the same time, in every cluster.
Three nodes of 16 GB use 48 GB and stay free; three nodes of 32 GB use 96 GB and need a license.

## A commercial license is needed if

- your nodes in production use more than 64 GB of RAM in total, whatever your revenue;
- your organization's revenue was 1,000,000 euros or more in its last fiscal year and you run
  TeselaDB in production for a commercial purpose; or
- you offer TeselaDB, modified or not, to third parties as a hosted or managed service, whatever
  your size.

## Every version becomes open source

Four years after a version of TeselaDB is published, that version is also licensed under the Apache
License 2.0 or the GNU GPL 2.0 or later, at your choice, with no conditions on production use. The
BUSL applies separately to each version, so a version published on 1 March 2027 becomes open source
on 1 March 2031.

## What the commercial license includes

- production use beyond the free limits, for the organizations it names and the RAM it covers;
- the right to offer TeselaDB as a hosted or managed service, if agreed;
- the versions published during the license term, with their updates;
- support terms, if agreed.

The price goes by the total RAM of your nodes and the rights agreed. To ask for a license, contact
VIUZI ESP through https://teseladb.com/pricing, saying who your organization is, how much RAM your
deployment uses and how you plan to use TeselaDB.
