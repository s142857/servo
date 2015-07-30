/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use cssparser::{Parser, Token, SourcePosition};
use properties::DeclaredValue;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use string_cache::Atom;

pub struct Value {
    /// In CSS syntax
    value: String,

    /// Custom property names in var() functions. Do not include the `--` prefix.
    references: HashSet<Atom>,
}

pub struct BorrowedValue<'a> {
    value: &'a str,
    references: Option<&'a HashSet<Atom>>,
}

pub fn parse(input: &mut Parser) -> Result<Value, ()> {
    let start = input.position();
    let mut references = HashSet::new();
    try!(parse_declaration_value(input, &mut references));
    Ok(Value {
        value: input.slice_from(start).to_owned(),
        references: references,
    })
}

/// https://drafts.csswg.org/css-syntax-3/#typedef-declaration-value
fn parse_declaration_value(input: &mut Parser, references: &mut HashSet<Atom>) -> Result<(), ()> {
    if input.is_exhausted() {
        // Need at least one token
        return Err(())
    }
    while let Ok(token) = input.next() {
        match token {
            Token::BadUrl |
            Token::BadString |
            Token::CloseParenthesis |
            Token::CloseSquareBracket |
            Token::CloseCurlyBracket |

            Token::Semicolon |
            Token::Delim('!') => {
                return Err(())
            }

            Token::Function(ref name) if name == "var" => {
                try!(input.parse_nested_block(|input| {
                    parse_var_function(input, references)
                }));
            }

            Token::Function(_) |
            Token::ParenthesisBlock |
            Token::CurlyBracketBlock |
            Token::SquareBracketBlock => {
                try!(input.parse_nested_block(|input| {
                    parse_declaration_value_block(input, references)
                }));
            }

            _ => {}
        }
    }
    Ok(())
}

/// Like parse_declaration_value,
/// but accept `!` and `;` since they are only invalid at the top level
fn parse_declaration_value_block(input: &mut Parser, references: &mut HashSet<Atom>)
                                 -> Result<(), ()> {
    while let Ok(token) = input.next() {
        match token {
            Token::BadUrl |
            Token::BadString |
            Token::CloseParenthesis |
            Token::CloseSquareBracket |
            Token::CloseCurlyBracket => {
                return Err(())
            }

            Token::Function(ref name) if name == "var" => {
                try!(input.parse_nested_block(|input| {
                    parse_var_function(input, references)
                }));
            }

            Token::Function(_) |
            Token::ParenthesisBlock |
            Token::CurlyBracketBlock |
            Token::SquareBracketBlock => {
                try!(input.parse_nested_block(|input| {
                    parse_declaration_value_block(input, references)
                }));
            }

            _ => {}
        }
    }
    Ok(())
}

// If the var function is valid, return Ok((custom_property_name, fallback))
fn parse_var_function<'i, 't>(input: &mut Parser<'i, 't>, references: &mut HashSet<Atom>)
                              -> Result<(), ()> {
    // https://drafts.csswg.org/css-variables/#typedef-custom-property-name
    let name = try!(input.expect_ident());
    let name = if name.starts_with("--") {
        &name[2..]
    } else {
        return Err(())
    };
    if input.expect_comma().is_ok() {
        try!(parse_declaration_value(input, references));
    }
    references.insert(Atom::from_slice(name));
    Ok(())
}

/// Add one custom property declaration to a map,
/// unless another with the same name was already there.
pub fn cascade<'a>(custom_properties: &mut Option<HashMap<&'a Atom, BorrowedValue<'a>>>,
                   inherited_custom_properties: &'a Option<Arc<HashMap<Atom, String>>>,
                   seen: &mut HashSet<&'a Atom>,
                   name: &'a Atom,
                   value: &'a DeclaredValue<Value>) {
    let was_not_already_present = seen.insert(name);
    if was_not_already_present {
        let map = match *custom_properties {
            Some(ref mut map) => map,
            None => {
                *custom_properties = Some(match *inherited_custom_properties {
                    Some(ref inherited) => inherited.iter().map(|(key, value)| {
                        (key, BorrowedValue { value: &value, references: None })
                    }).collect(),
                    None => HashMap::new(),
                });
                custom_properties.as_mut().unwrap()
            }
        };
        match *value {
            DeclaredValue::Value(ref value) => {
                map.insert(name, BorrowedValue {
                    value: &value.value,
                    references: Some(&value.references),
                });
            }
            DeclaredValue::Initial => {
                map.remove(&name);
            }
            DeclaredValue::Inherit => {}  // The inherited value is what we already have.
        }
    }
}

pub fn finish_cascade(custom_properties: Option<HashMap<&Atom, BorrowedValue>>,
                      inherited_custom_properties: &Option<Arc<HashMap<Atom, String>>>)
                      -> Option<Arc<HashMap<Atom, String>>> {
    if let Some(custom_properties) = custom_properties {
        let mut invalid = HashSet::new();
        find_cycles(&custom_properties, &mut invalid);
        let mut substituted_map = HashMap::new();
        for (&name, value) in &custom_properties {
            // If this value is invalid at computed time it won’t be inserted in substituted_map.
            // Nothing else to do.
            let _ = substitute_one(
                name, value, &custom_properties, None, &mut substituted_map, &mut invalid);
        }
        Some(Arc::new(substituted_map))
    } else {
        // At most this clones an `Arc` (i.e. increments a reference count).
        // Making this cheap is why we use `Arc` at all.
        inherited_custom_properties.clone()
    }
}

/// https://drafts.csswg.org/css-variables/#cycles
fn find_cycles(map: &HashMap<&Atom, BorrowedValue>, invalid: &mut HashSet<Atom>) {
    let mut visited = HashSet::new();
    let mut stack = Vec::new();
    for name in map.keys() {
        walk(map, name, &mut stack, &mut visited, invalid);

        fn walk<'a>(map: &HashMap<&'a Atom, BorrowedValue<'a>>,
                    name: &'a Atom,
                    stack: &mut Vec<&'a Atom>,
                    visited: &mut HashSet<&'a Atom>,
                    invalid: &mut HashSet<Atom>) {
            let was_not_already_present = visited.insert(name);
            if !was_not_already_present {
                return
            }
            if let Some(value) = map.get(name) {
                if let Some(references) = value.references {
                    stack.push(name);
                    for next in references {
                        if let Some(position) = stack.position_elem(&next) {
                            // Found a cycle
                            for in_cycle in &stack[position..] {
                                invalid.insert((**in_cycle).clone());
                            }
                        } else {
                            walk(map, next, stack, visited, invalid);
                        }
                    }
                    stack.pop();
                }
            }
        }
    }
}

fn substitute_one(name: &Atom,
                  value: &BorrowedValue,
                  custom_properties: &HashMap<&Atom, BorrowedValue>,
                  substituted: Option<&mut String>,
                  substituted_map: &mut HashMap<Atom, String>,
                  invalid: &mut HashSet<Atom>)
                  -> Result<(), ()> {
    if let Some(value) = substituted_map.get(name) {
        if let Some(substituted) = substituted {
            substituted.push_str(value)
        }
        return Ok(())
    }

    if invalid.contains(name) {
        return Err(());
    }
    let value = if let Some(references) = value.references {
        if !references.is_empty() {
            let mut substituted = String::new();
            let mut input = Parser::new(&value.value);
            let mut start = input.position();
            if substitute_block(
                custom_properties, &mut input, &mut start, &mut substituted,
                substituted_map, invalid,
            ).is_err() {
                invalid.insert(name.clone());
                return Err(())
            }
            substituted.push_str(input.slice_from(start));
            substituted
        } else {
            value.value.to_owned()
        }
    } else {
        value.value.to_owned()
    };
    if let Some(substituted) = substituted {
        substituted.push_str(&value)
    }
    substituted_map.insert(name.clone(), value);
    Ok(())
}

fn substitute_block(custom_properties: &HashMap<&Atom, BorrowedValue>,
                    input: &mut Parser,
                    start: &mut SourcePosition,
                    substituted: &mut String,
                    substituted_map: &mut HashMap<Atom, String>,
                    invalid: &mut HashSet<Atom>)
                    -> Result<(), ()> {
    while let Ok(token) = input.next() {
        match token {
            Token::Function(ref name) if name == "var" => {
                substituted.push_str(input.slice_from(*start));
                try!(input.parse_nested_block(|input| {
                    let name = input.expect_ident().unwrap();
                    debug_assert!(name.starts_with("--"));
                    let name = Atom::from_slice(&name[2..]);

                    if let Some(value) = custom_properties.get(&name) {
                        try!(substitute_one(
                            &name, value, custom_properties,
                            Some(substituted), substituted_map, invalid));
                        // Skip over the fallback, as `parse_nested_block` would return `Err`
                        // if we don’t consume all of `input`.
                        // FIXME: Add a specialized method to cssparser to do this with less work.
                        while let Ok(_) = input.next() {}
                    } else {
                        try!(input.expect_comma());
                        let mut start = input.position();
                        try!(substitute_block(
                            custom_properties, input, &mut start, substituted,
                            substituted_map, invalid));
                        substituted.push_str(input.slice_from(start));
                    }
                    Ok(())
                }));
                *start = input.position();
            }

            Token::Function(_) |
            Token::ParenthesisBlock |
            Token::CurlyBracketBlock |
            Token::SquareBracketBlock => {
                try!(input.parse_nested_block(|input| substitute_block(
                    custom_properties, input, start, substituted, substituted_map, invalid)));
            }

            _ => {}
        }
    }
    Ok(())
}
