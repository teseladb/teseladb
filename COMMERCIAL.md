# Licensing TeselaDB

TeselaDB is source-available under the [Business Source License 1.1](LICENSE) (BSL). Here you have, in plain words,
what you can do for free and when you need a commercial license. If this page and [LICENSE](LICENSE) ever disagree,
LICENSE is what applies.

## What you can always do

You can read the source code, build it, modify it, create derivative works and redistribute them, and use TeselaDB
outside production: to develop, test, evaluate, study or demonstrate it, and in continuous integration. Every copy you
redistribute, modified or not, keeps this license.

## When production use is free

You can run TeselaDB in production for free, including in your business and for the work you do for your customers,
when all four conditions of this list hold at the same time.

- Its instances use 64 GB of RAM or less in total.
- They use 16 vCPUs or less in total.
- Your organization, together with every company that controls it, is controlled by it or shares its owner, had less
  than 1,000,000 euros of revenue in its last completed fiscal year.
- You do not offer TeselaDB to third parties as a database service.

## Offering TeselaDB as a service always needs a license

A database service lets people outside your organization create databases, collections or clusters whose data they
control: a database as a service, a database platform or a managed cluster. Offering one built on TeselaDB always
needs a commercial license that grants it, whatever your RAM, vCPUs and revenue. Running TeselaDB as the database of
your own applications, even when your customers use those applications, is not a database service.

## How RAM and vCPUs are counted

The RAM of an instance is the memory its process can use: the limit of its container or control group (`--memory` of
Docker, `limits.memory` of Kubernetes, `memory.max` of cgroup v2), or the total memory of the machine when there is no
limit, rounded up to the whole gigabyte. Its vCPUs are the CPU quota of its container or control group, rounded up to
the whole processor, or the online logical processors of the machine when there is no quota; each hardware thread
counts as one. The totals add up every instance your organization runs in production at the same time, in every
cluster. Three nodes of 4 vCPUs and 16 GB use 12 vCPUs and 48 GB, and stay free if your revenue is under 1,000,000
euros. Three nodes of 8 vCPUs and 32 GB use 24 vCPUs and 96 GB, and need a license whatever your revenue.

## A commercial license is needed if

- your instances in production use more than 64 GB of RAM or more than 16 vCPUs in total;
- your organization's revenue was 1,000,000 euros or more in its last fiscal year and you run TeselaDB in production;
  or
- you offer TeselaDB to third parties as a database service, whatever your size.

## Each version becomes Apache 2.0

Four years after a version is first published, that version is also available under the Apache License, Version 2.0.
Version 0.1.13 changes on 13 November 2030. The versions 0.1.0 to 0.1.12, first published under the TeselaDB License
1.0, are available under this license too, each with its change date four years after its own publication.

## What the commercial license includes

- production use beyond the free limits, for the organizations it names and the RAM and vCPUs it covers;
- the right to offer TeselaDB as a database service, if agreed;
- the versions published during the license term, with their updates;
- support terms, if agreed.

The price goes by the total RAM and vCPUs of your instances and the rights agreed. To ask for a license, contact
VIUZI ESP S.L. through https://teseladb.com/pricing, saying who your organization is, how much RAM and how many vCPUs
your deployment uses and how you plan to use TeselaDB.
