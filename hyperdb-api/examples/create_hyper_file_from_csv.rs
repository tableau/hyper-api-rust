// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Example: Create a Hyper file from CSV
//!
//! This example demonstrates how to:
//! - Load data from a CSV file into a new Hyper file
//! - Use the SQL COPY command to import CSV data
//! - Configure process and connection parameters
//!
//! This is a Rust port of the C++ example `create_hyper_file_from_csv.cpp`.
//!
//! Run with: cargo run -p hyperdb-api --example `create_hyper_file_from_csv`

use hyperdb_api::{
    Catalog, Connection, CreateMode, HyperProcess, Parameters, Result, SqlType, TableDefinition,
    escape_string_literal,
};

/// Creates the Customer table definition.
/// Since the table name is not prefixed with an explicit schema name,
/// the table will reside in the default "public" namespace.
fn customer_table() -> TableDefinition {
    TableDefinition::from("Customer")
        .add_required_column("Customer ID", SqlType::text())
        .add_required_column("Customer Name", SqlType::text())
        .add_required_column("Loyalty Reward Points", SqlType::big_int())
        .add_required_column("Segment", SqlType::text())
}

fn run_create_hyper_file_from_csv() -> Result<()> {
    println!("EXAMPLE - Load data from CSV into table in new Hyper file");

    // Create test_results directory if it doesn't exist
    std::fs::create_dir_all("test_results")?;

    let path_to_database = "test_results/create_hyper_file_from_csv.hyper";

    // Create a sample CSV file for this example
    let path_to_csv = "test_results/customers.csv";
    create_sample_csv(path_to_csv)?;

    {
        // Optional process parameters. They are documented in the Tableau Hyper documentation,
        // chapter "Process Settings"
        // (https://tableau.github.io/hyper-db/docs/hyper-api/hyper_process#process-settings).
        let mut process_parameters = Parameters::new();
        process_parameters.set("log_file_max_count", "2"); // Limits the number of Hyper event log files to two.
        process_parameters.set("log_file_size_limit", "100M"); // Limits the size of Hyper event log files to 100 megabytes.
        process_parameters.set("log_dir", "test_results");

        let hyper = HyperProcess::new(None, Some(&process_parameters))?;

        // Creates new Hyper file "customer.hyper".
        // Replaces existing file if it already exists.
        {
            let connection =
                Connection::new(&hyper, path_to_database, CreateMode::CreateAndReplace)?;
            let catalog = Catalog::new(&connection);

            let customer_def = customer_table();
            catalog.create_table(&customer_def)?;

            // Load all rows into "Customer" table from the CSV file.
            // `execute_command` executes a SQL statement and returns the impacted row count.
            //
            // Note:
            // You might have to adjust the COPY parameters to the format of your specific csv file.
            // The example assumes that your columns are separated with the ',' character
            // and that NULL values are encoded via the string 'NULL'.
            // Also be aware that the `header` option is used in this example:
            // It treats the first line of the csv file as a header and does not import it.
            //
            // The parameters of the COPY command are documented in the Data Cloud SQL reference (the SQL dialect Hyper uses)
            // (https://developer.salesforce.com/docs/data/data-cloud-query-guide/references/dc-sql-reference/copy-from.html).
            println!(
                "Issuing the SQL COPY command to load the csv file into the table. Since the first line"
            );
            println!(
                "of our csv file contains the column names, we use the `header` option to skip it."
            );

            let copy_command = format!(
                "COPY {} FROM {} WITH (FORMAT csv, NULL 'NULL', DELIMITER ',', HEADER)",
                customer_def.qualified_name(),
                escape_string_literal(path_to_csv)
            );

            let row_count = connection.execute_command(&copy_command)?;
            println!("The number of rows in table \"Customer\" is {row_count}.");
        }
        println!("The connection to the Hyper file has been closed.");
    }
    println!("The Hyper Process has been shut down.");

    Ok(())
}

/// Creates a sample CSV file for this example.
fn create_sample_csv(path: &str) -> std::io::Result<()> {
    use std::io::Write;

    // Create parent directory if needed
    if let Some(parent) = std::path::Path::new(path).parent() {
        std::fs::create_dir_all(parent)?;
    }

    let mut file = std::fs::File::create(path)?;
    writeln!(
        file,
        "Customer ID,Customer Name,Loyalty Reward Points,Segment"
    )?;
    writeln!(file, "DK-13375,Dennis Kane,518,Consumer")?;
    writeln!(file, "EB-13705,Ed Braxton,815,Corporate")?;
    writeln!(file, "JL-15235,Jane Lopez,2100,Consumer")?;
    writeln!(file, "MK-16790,Mike Kelly,320,Corporate")?;
    writeln!(file, "SR-20815,Sarah Roberts,1450,Home Office")?;

    Ok(())
}

fn main() {
    match run_create_hyper_file_from_csv() {
        Ok(()) => {}
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    }
}
