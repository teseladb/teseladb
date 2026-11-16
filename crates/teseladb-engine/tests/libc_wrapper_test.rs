//! Tests of the `getgrnam` wrapper: an unknown group gives `None` and a known one its details.

#![cfg(target_os = "linux")]

use teseladb::engine::core::coroutine::make_coroutine;
use teseladb::engine::core::file::GroupDetails;
use teseladb::engine::util::file::getgrnam;
use teseladb::engine_test_case;

engine_test_case!(getgrnam_group_name_does_not_exist_test, {
    make_coroutine(async move {
        // A better approach would be to use a test setup and teardown to create a fake group
        // with members in it and in the teardown delete the fake group.
        let grp: Option<GroupDetails> = getgrnam("roo").await?;
        assert!(grp.is_none());
        Ok(())
    })
});

engine_test_case!(getgrnam_group_name_exists_test, {
    make_coroutine(async move {
        let grp: Option<GroupDetails> = getgrnam("root").await?;
        assert!(grp.is_some());
        assert_eq!(grp.as_ref().map(|g| g.group_name.as_str()), Some("root"));
        Ok(())
    })
});
