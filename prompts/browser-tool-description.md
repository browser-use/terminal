Compatibility browser status/admin surface for the raw browser-harness MVP.

Use `browser_script` for browser work. The old Rust browser control plane is
disabled. Browser lifecycle is owned by browser-harness and is controlled from
Python with helpers such as:

```python
browser_new("private")
browser_new("cloud")
browser(id)
browser_list()
browser_status(id)
browser_close(id)
browser_close_owned()
browser_profiles()
browser_use_profile(profile_id)
```

This tool may report stored terminal preferences or a concise browser-harness
status, but it does not connect, recover, launch, or manage browsers itself.
