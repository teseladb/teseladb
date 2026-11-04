# Licensing TeselaDB

TeselaDB is distributed in object code (the binary, the Docker images and the install script) under the
[TeselaDB License](LICENSE). This page explains, in plain words, who can use it for free and who needs a
commercial license. If this page and [LICENSE](LICENSE) ever disagree, LICENSE is what applies.

## Free for you, also for commercial use, if

You can run TeselaDB in production for free, including in your business and for your customers' work, when at least
one of these holds:

- its nodes use **32 GB of RAM or less** in total, whatever your revenue;
- your organization, together with every company that controls it, is controlled by it or shares its owner, had **less
  than 1,000,000 euros of revenue** in its last completed fiscal year, whatever the RAM; or
- your use is non-commercial: personal projects, education, academic research, a non-profit organization or a public
  administration.

Developing, testing, evaluating or demonstrating TeselaDB, and running it in continuous integration without data of a
production system, is always free.

In every case you must not offer TeselaDB to third parties as a hosted or managed service, such as selling a database
service built on it. Running TeselaDB as the database of your own applications, even when your customers use those
applications, is not a hosted service.

## What the license does not allow

You may not modify TeselaDB, reverse engineer, decompile or disassemble it, sell or sublicense it, or distribute it to
third parties or inside a product you distribute, without a commercial license. Copying the unmodified binary inside
your organization and pulling the official images is allowed. The rights the law grants every lawful user, such as a
back-up copy or decompiling for interoperability under Directive 2009/24/EC, stay intact.

## How the RAM is counted

The RAM of a node is the memory its process can use: the limit of its container or control group (`--memory` of Docker,
`limits.memory` of Kubernetes, `memory.max` of cgroup v2), or the total memory of the machine when there is no limit,
rounded up to the whole gigabyte. The total is the sum over every node of TeselaDB that your organization runs in
production at the same time, in every cluster. Three nodes of 8 GB use 24 GB and stay free whatever your revenue;
three nodes of 16 GB use 48 GB and stay free only if your revenue is under 1,000,000 euros.

## A commercial license is needed if

- your nodes in production use more than 32 GB of RAM in total **and** your organization's revenue was 1,000,000 euros
  or more in its last fiscal year, for a commercial use;
- you offer TeselaDB to third parties as a hosted or managed service, whatever your size; or
- you distribute TeselaDB to third parties, or ship it inside your own product.

## What the commercial license includes

- production use beyond the free limits, for the organizations it names and the RAM it covers;
- the right to offer TeselaDB as a hosted or managed service, or to ship it in your product, if agreed;
- the versions published during the license term, with their updates;
- support terms and, for enterprise customers, escrow of the source code, if agreed.

The price goes by the total RAM of your nodes and the rights agreed. To ask for a license, contact VIUZI ESP through
https://teseladb.com/pricing, saying who your organization is, how much RAM your deployment uses and how you plan to use
TeselaDB.
