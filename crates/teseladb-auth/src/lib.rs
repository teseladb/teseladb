//! Authentication and authorization of TeselaDB: SCRAM, SASLprep, X.509, users, roles, privileges and auditing.
#![forbid(unsafe_code)]
#![cfg_attr(coverage, feature(coverage_attribute))]

pub mod actions;
pub mod apikey;
pub mod audit;
pub mod audit_crypt;
pub mod audit_ocsf;
pub mod audit_param;
pub mod authz;
pub mod authz_rules;
mod authz_users;
pub mod aws;
pub mod bearer;
pub mod builtin_roles;
pub mod clustertime;
pub mod fence;
pub mod kerberos;
pub mod keyfile;
pub mod kmip;
pub mod ldap;
pub mod ldap_ber;
pub mod ldap_filter;
pub mod lease_driver;
pub mod oidc;
pub mod plain;
pub mod privileges;
pub mod restrictions;
pub mod roles;
pub mod sasl;
pub mod saslprep;
pub mod scram;
pub mod shell_json;
mod stringprep_tables;
pub mod users;
