use std::io;
use std::io::{BufRead, BufReader};
use std::path::{PathBuf, Path};
use std::ffi::CString;
use std::ops::Deref;
use std::fs::File;
use std::collections::HashSet;
use object::Object;
use object::File as OFile;
use memmap::{Mmap, Protection};
use gimli::*;
use regex::Regex;
use rustc_demangle::demangle;

/// Describes a function as low_pc, high_pc and bool representing is_test.
type FuncDesc = (u64, u64, FunctionType);

#[derive(Clone, Copy, PartialEq)]
enum FunctionType {
    Generated,
    Test,
    Standard
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LineType {
    /// Generated test main. Shouldn't be traced.
    TestMain,
    /// Entry of function known to be a test
    TestEntry(u64),
    /// Entry of function. May or may not be test
    FunctionEntry(u64),
    /// Standard statement
    Statement,
    /// Condition
    Condition,
    /// Unknown type
    Unknown,
}


#[derive(Debug, Clone)]
pub struct TracerData {
    pub path: PathBuf,
    pub line: u64,
    pub address: u64,
    pub trace_type: LineType,
    pub hits: u64,
}


fn line_is_traceable(file: &PathBuf, line: u64) -> bool {
    let mut result = false;
    if line > 0 {
        // Module imports are flagged as debuggable. But are always ran so meaningless!
        let reg: Regex = Regex::new(r"(:?^|\s)(:?mod)|(:?crate)\s+\w+;").unwrap();
        if let Ok(f) = File::open(file) {
            let reader = BufReader::new(&f);
            if let Some(Ok(l)) = reader.lines().nth((line - 1) as usize) {
                result = !reg.is_match(l.as_ref());
            }
        }
    }
    result
}

fn generate_func_desc<T: Endianity>(die: &DebuggingInformationEntry<T>,
                                    debug_str: &DebugStr<T>) -> Result<FuncDesc> {
    let mut func_type = FunctionType::Standard;
    let low = die.attr_value(DW_AT_low_pc)?;
    let high = die.attr_value(DW_AT_high_pc)?;
    let linkage = die.attr_value(DW_AT_linkage_name)?;

    // Low is a program counter address so stored in an Addr
    let low = match low {
        Some(AttributeValue::Addr(x)) => x,
        _ => 0u64,
    };
    // High is an offset from the base pc, therefore is u64 data.
    let high = match high {
        Some(AttributeValue::Udata(x)) => x,
        _ => 0u64,
    };
    if let Some(AttributeValue::DebugStrRef(offset)) = linkage {
        let empty = CString::new("").unwrap();
        let name = debug_str.get_str(offset).unwrap_or(empty.deref());
        // go from CStr to Owned String
        let name = name.to_str().unwrap_or("");
        let name = demangle(name).to_string();
        // Simplest test is whether it's in tests namespace.
        // Rust guidelines recommend all tests are in a tests module.
        func_type = if name.contains("tests::") {
            FunctionType::Test
        } else if name.contains("__test::main") {
            FunctionType::Generated
        } else {
            FunctionType::Standard
        };
    } 
    Ok((low, high, func_type))
}


/// Finds all function entry points and returns a vector
/// This will identify definite tests, but may be prone to false negatives.
/// TODO Potential to trace all function calls from __test::main and find addresses of interest
fn get_entry_points<T: Endianity>(debug_info: &CompilationUnitHeader<T>,
                                  debug_abbrev: &Abbreviations,
                                  debug_str: &DebugStr<T>) -> Vec<FuncDesc> {
    let mut result:Vec<FuncDesc> = Vec::new();
    let mut cursor = debug_info.entries(debug_abbrev);
    // skip compilation unit root.
    let _ = cursor.next_entry();
    while let Ok(Some((_, node))) = cursor.next_dfs() {
        // Function DIE
        if node.tag() == DW_TAG_subprogram {
            
            if let Ok(fd) = generate_func_desc(&node, &debug_str) {
                result.push(fd);
            }
        }
    }
    result
}

fn get_addresses_from_program<T:Endianity>(prog: IncompleteLineNumberProgram<T>,
                                           entries: &Vec<(u64, LineType)>,
                                           project: &Path) -> Result<Vec<TracerData>> {
    let mut result: Vec<TracerData> = Vec::new();
    let ( cprog, seq) = prog.sequences()?;
    for s in seq {
        let mut sm = cprog.resume_from(&s);   
         while let Ok(Some((ref header, &ln_row))) = sm.next_row() {
            if let Some(file) = ln_row.file(header) {
                let mut path = PathBuf::new();
                
                if let Some(dir) = file.directory(header) {
                    if let Ok(temp) = String::from_utf8(dir.to_bytes().to_vec()) {
                        path.push(temp);
                    }
                }
                if path.is_relative() {
                    if let Ok(p) = path.canonicalize() {
                        path = p;
                    }
                } 
                // Source is part of project so we cover it.
                if path.starts_with(project) {
                    let force_test = path.starts_with(project.join("tests"));
                    if let Some(file) = ln_row.file(header) {
                        // If we can't map to line, we can't trace it.
                        let line = ln_row.line().unwrap_or(0);
                        let file = file.path_name();
                        // We now need to filter out lines which are meaningless to trace.
                        
                        if let Ok(file) = String::from_utf8(file.to_bytes().to_vec()) {
                            path.push(file);
                            if !line_is_traceable(&path, line) {
                                continue;
                            }
                            let address = ln_row.address();
                            
                            let desc = entries.iter()
                                              .filter(|&&(addr, _)| addr == address )
                                              .map(|&(_, t)| t)
                                              .nth(0)
                                              .unwrap_or(LineType::Unknown);
                            // TODO HACK: If in tests/ directory force it to a TestEntry 
                            let desc = match desc {
                                LineType::FunctionEntry(e) if force_test => LineType::TestEntry(e),
                                x @ _ => x
                            };
                            result.push( TracerData {
                                path: path,
                                line: line,
                                address: address,
                                trace_type: desc,
                                hits: 0u64
                            });
                        }
                    }
                }
            }
        }
    }
    Ok(result)
}

fn get_line_addresses<Endian: Endianity>(project: &Path, obj: &OFile) -> Result<Vec<TracerData>>  {
    let mut result: Vec<TracerData> = Vec::new();
    let debug_info = obj.get_section(".debug_info").unwrap_or(&[]);
    let debug_info = DebugInfo::<Endian>::new(debug_info);
    let debug_abbrev = obj.get_section(".debug_abbrev").unwrap_or(&[]);
    let debug_abbrev = DebugAbbrev::<Endian>::new(debug_abbrev);
    let debug_strings = obj.get_section(".debug_str").unwrap_or(&[]);
    let debug_strings = DebugStr::<Endian>::new(debug_strings);

    let mut iter = debug_info.units();
    while let Ok(Some(cu)) = iter.next() {
        let addr_size = cu.address_size();
        let abbr = match cu.abbreviations(debug_abbrev) {
            Ok(a) => a,
            _ => continue,
        };
        let entries = get_entry_points(&cu, &abbr, &debug_strings)
            .iter()
            .map(|&(a, b, c)| { 
                match c {
                    FunctionType::Test => (a, LineType::TestEntry(b)),
                    FunctionType::Standard => (a, LineType::FunctionEntry(b)),
                    FunctionType::Generated => (a, LineType::TestMain),
                }
            }).collect();
        if let Ok(Some((_, root))) = cu.entries(&abbr).next_dfs() {
            let offset = match root.attr_value(DW_AT_stmt_list) {
                Ok(Some(AttributeValue::DebugLineRef(o))) => o,
                _ => continue,
            };
            let debug_line = obj.get_section(".debug_line").unwrap_or(&[]);
            let debug_line = DebugLine::<Endian>::new(debug_line);
            
            let prog = debug_line.program(offset, addr_size, None, None)?; 
            if let Ok(mut addresses) = get_addresses_from_program(prog, &entries, &project) {
                result.append(&mut addresses);
            }
        }
    }
    // Due to rust being a higher level language multiple instructions may map
    // to the same line. This prunes these to just the first instruction address
    let mut check: HashSet<(&Path, u64)> = HashSet::new();
    let mut result = result.iter()
                           .filter(|x| check.insert((x.path.as_path(), x.line)))
                           .filter(|x| x.trace_type != LineType::TestMain)
                           .map(|x| x.clone())
                           .collect::<Vec<TracerData>>();
    let addresses = result.iter()
                          .map(|x| x.address)
                          .collect::<Vec<_>>();
    // TODO Probably a more idiomatic way to do this.
    {
        let test_entries = result.iter_mut()
                                 .filter(|x| match x.trace_type {
                                     LineType::TestEntry(_) => true,
                                     _ => false,
                                 });

        for ref mut test_entry in test_entries {
            let endpoint = match test_entry.trace_type {
                LineType::TestEntry(x) => x,
                _ => continue,
            };
            let max_address = addresses.iter()
                                       .fold(0, |acc, x| {
                                           if x > &(test_entry.address + acc) && (x-test_entry.address) <= endpoint {
                                               *x - test_entry.address
                                           } else { 
                                               acc  
                                           }
                                       });
            test_entry.trace_type = LineType::TestEntry(max_address);
        }
    }
    Ok(result)
}


/// Generates a list of lines we want to trace the coverage of. Used to instrument the
/// traces into the test executable
pub fn generate_tracer_data(manifest: &Path, test: &Path) -> io::Result<Vec<TracerData>> {
    let file = File::open(test)?;
    let file = Mmap::open(&file, Protection::Read)?;
    if let Ok(obj) = OFile::parse(unsafe {file.as_slice() }) {
        
        let data = if obj.is_little_endian() {
            get_line_addresses::<LittleEndian>(manifest, &obj)
        } else {
            get_line_addresses::<BigEndian>(manifest, &obj)
        };
        data.map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "Error while parsing"))
    } else {
        Err(io::Error::new(io::ErrorKind::InvalidData, "Unable to parse binary."))
    }
}
