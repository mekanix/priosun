use anyhow::{bail, Result};
use nvtree::{nvtree_find, Nvtree, Nvtvalue};
use tabled::Tabled;

#[derive(Tabled)]
struct NameRow {
    name: String,
}

#[derive(Tabled)]
struct JailRow {
    name: String,
    hostname: String,
    ips: String,
    status: String,
}

#[derive(Tabled)]
struct VmRow {
    name: String,
    status: String,
}

pub fn nvtree_to_table(response: &Nvtree) -> Result<String> {
    let table = if let Some(rows) = rows(response, "datasets") {
        tabled::Table::new(rows.iter().map(name_row).collect::<Vec<_>>()).to_string()
    } else if let Some(rows) = rows(response, "volumes") {
        tabled::Table::new(rows.iter().map(name_row).collect::<Vec<_>>()).to_string()
    } else if let Some(rows) = rows(response, "jails") {
        tabled::Table::new(
            rows.iter()
                .map(|row| JailRow {
                    name: string(row, "name"),
                    hostname: string(row, "hostname"),
                    ips: string(row, "ips"),
                    status: string(row, "status"),
                })
                .collect::<Vec<_>>(),
        )
        .to_string()
    } else if let Some(rows) = rows(response, "vms") {
        tabled::Table::new(
            rows.iter()
                .map(|row| VmRow {
                    name: string(row, "name"),
                    status: string(row, "status"),
                })
                .collect::<Vec<_>>(),
        )
        .to_string()
    } else {
        bail!("response contains no tabular resource")
    };
    Ok(format!("{table}\n"))
}

fn rows<'a>(response: &'a Nvtree, field: &str) -> Option<&'a [Nvtree]> {
    match nvtree_find(response, field).map(|pair| &pair.value) {
        Some(Nvtvalue::NestedArray(rows)) => Some(rows),
        _ => None,
    }
}

fn name_row(row: &Nvtree) -> NameRow {
    NameRow {
        name: string(row, "name"),
    }
}

fn string(row: &Nvtree, field: &str) -> String {
    match nvtree_find(row, field).map(|pair| &pair.value) {
        Some(Nvtvalue::String(value)) => value.clone(),
        _ => String::new(),
    }
}
