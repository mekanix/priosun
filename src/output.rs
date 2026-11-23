use anyhow::{Result, bail};
use nvtree::{Nvtree, Nvtvalue, nvtree_find};
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

pub fn nvtree_to_columns(
    response: &Nvtree,
    resource: &str,
    fields: &[String],
    headers: bool,
) -> Result<String> {
    let rows = rows(response, resource)
        .ok_or_else(|| anyhow::anyhow!("response contains no {resource}"))?;
    let columns = rows
        .iter()
        .map(|row| fields.iter().map(|field| string(row, field)).collect())
        .collect::<Vec<Vec<String>>>();
    Ok(rows_to_columns(
        fields
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .as_slice(),
        &columns,
        &(0..fields.len()).collect::<Vec<_>>(),
        headers,
    ))
}

pub fn rows_to_columns(
    names: &[&str],
    rows: &[Vec<String>],
    fields: &[usize],
    headers: bool,
) -> String {
    let mut output = String::new();
    if headers {
        output.push_str(
            &fields
                .iter()
                .map(|field| names[*field])
                .collect::<Vec<_>>()
                .join("\t"),
        );
        output.push('\n');
    }
    for row in rows {
        output.push_str(
            &fields
                .iter()
                .map(|field| row[*field].as_str())
                .collect::<Vec<_>>()
                .join("\t"),
        );
        output.push('\n');
    }
    output
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
