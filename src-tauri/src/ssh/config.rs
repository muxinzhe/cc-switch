use std::collections::BTreeSet;
use std::path::Path;

pub(super) fn aliases(home: &Path) -> Result<Vec<String>, String> {
    let root = home.join(".ssh");
    let mut hosts = BTreeSet::new();
    read_config(&root.join("config"), &root, home, &mut hosts, 0)?;
    Ok(hosts.into_iter().collect())
}

fn read_config(
    path: &Path,
    root: &Path,
    home: &Path,
    hosts: &mut BTreeSet<String>,
    depth: usize,
) -> Result<(), String> {
    // OpenSSH caps nested Includes at 16. Includes are relative to ~/.ssh,
    // including those encountered in a file in one of its subdirectories.
    if depth > 16 {
        return Err(format!(
            "Too many nested SSH Includes at {}",
            path.display()
        ));
    }
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("{}: {error}", path.display())),
    };

    for line in text.lines() {
        let line = line.trim_start();
        let Some(split) = line.find(|ch: char| ch.is_ascii_whitespace() || ch == '=') else {
            continue;
        };
        let keyword = &line[..split];
        let values = line[split..].trim_start();
        let values = values.strip_prefix('=').unwrap_or(values);
        let Some(words) = shlex::split(values) else {
            continue;
        };

        if keyword.eq_ignore_ascii_case("host") {
            hosts.extend(
                words
                    .into_iter()
                    .filter(|name| valid_destination(name) && !name.starts_with('!')),
            );
        } else if keyword.eq_ignore_ascii_case("include") {
            for pattern in words {
                let expanded = match pattern.strip_prefix("~/") {
                    Some(path) => home.join(path),
                    None => root.join(pattern),
                };
                let pattern = expanded.to_str().ok_or("Invalid SSH Include path")?;
                let include_files = glob::glob(pattern).map_err(|e| e.to_string())?;
                for include in include_files {
                    let include = include.map_err(|e| e.to_string())?;
                    read_config(&include, root, home, hosts, depth + 1)?;
                }
            }
        }
    }
    Ok(())
}

pub(super) fn valid_destination(destination: &str) -> bool {
    !destination.is_empty()
        && !destination.starts_with('-')
        && destination
            .chars()
            .all(|ch| ch.is_alphanumeric() || matches!(ch, '.' | '_' | '-' | ':' | '@' | '[' | ']'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn imports_concrete_aliases_and_nested_includes() {
        let home = tempfile::tempdir().unwrap();
        let root = home.path().join(".ssh");
        fs::create_dir_all(root.join("conf.d")).unwrap();
        fs::write(
            root.join("config"),
            "Host app-* !app-test staging\ninclude=conf.d/*.conf\nHost \"work\" staging # comment\n",
        )
        .unwrap();
        fs::write(
            root.join("conf.d/one.conf"),
            "HOST = dev\nInclude ~/.ssh/two\nHostName example.invalid\n",
        )
        .unwrap();
        fs::write(root.join("two"), "Host third\n").unwrap();

        assert_eq!(
            aliases(home.path()).unwrap(),
            ["dev", "staging", "third", "work"]
        );
        assert!(!valid_destination("-F"));
        assert!(!valid_destination("dev\nexample"));
        assert!(valid_destination("deploy@192.0.2.10"));
    }
}
