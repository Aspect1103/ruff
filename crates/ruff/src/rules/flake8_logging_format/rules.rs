use ruff_text_size::{TextRange, TextSize};
use rustpython_parser::ast::{self, Constant, Expr, ExprKind, Keyword, Operator};

use ruff_diagnostics::{Diagnostic, Edit, Fix};
use ruff_python_ast::helpers::{find_keyword, SimpleCallArgs};
use ruff_python_semantic::analyze::logging;
use ruff_python_stdlib::logging::LoggingLevel;

use crate::checkers::ast::Checker;
use crate::registry::{AsRule, Rule};
use crate::rules::flake8_logging_format::violations::{
    LoggingExcInfo, LoggingExtraAttrClash, LoggingFString, LoggingPercentFormat,
    LoggingRedundantExcInfo, LoggingStringConcat, LoggingStringFormat, LoggingWarn,
};

const RESERVED_ATTRS: &[&str; 22] = &[
    "args",
    "asctime",
    "created",
    "exc_info",
    "exc_text",
    "filename",
    "funcName",
    "levelname",
    "levelno",
    "lineno",
    "module",
    "msecs",
    "message",
    "msg",
    "name",
    "pathname",
    "process",
    "processName",
    "relativeCreated",
    "stack_info",
    "thread",
    "threadName",
];

/// Check logging messages for violations.
fn check_msg(checker: &mut Checker, msg: &Expr) {
    match &msg.node {
        // Check for string concatenation and percent format.
        ExprKind::BinOp(ast::ExprBinOp { op, .. }) => match op {
            Operator::Add => {
                if checker.settings.rules.enabled(Rule::LoggingStringConcat) {
                    checker
                        .diagnostics
                        .push(Diagnostic::new(LoggingStringConcat, msg.range()));
                }
            }
            Operator::Mod => {
                if checker.settings.rules.enabled(Rule::LoggingPercentFormat) {
                    checker
                        .diagnostics
                        .push(Diagnostic::new(LoggingPercentFormat, msg.range()));
                }
            }
            _ => {}
        },
        // Check for f-strings.
        ExprKind::JoinedStr(_) => {
            if checker.settings.rules.enabled(Rule::LoggingFString) {
                checker
                    .diagnostics
                    .push(Diagnostic::new(LoggingFString, msg.range()));
            }
        }
        // Check for .format() calls.
        ExprKind::Call(ast::ExprCall { func, .. }) => {
            if checker.settings.rules.enabled(Rule::LoggingStringFormat) {
                if let ExprKind::Attribute(ast::ExprAttribute { value, attr, .. }) = &func.node {
                    if attr == "format" && matches!(value.node, ExprKind::Constant(_)) {
                        checker
                            .diagnostics
                            .push(Diagnostic::new(LoggingStringFormat, msg.range()));
                    }
                }
            }
        }
        _ => {}
    }
}

/// Check contents of the `extra` argument to logging calls.
fn check_log_record_attr_clash(checker: &mut Checker, extra: &Keyword) {
    match &extra.node.value.node {
        ExprKind::Dict(ast::ExprDict { keys, .. }) => {
            for key in keys {
                if let Some(key) = &key {
                    if let ExprKind::Constant(ast::ExprConstant {
                        value: Constant::Str(string),
                        ..
                    }) = &key.node
                    {
                        if RESERVED_ATTRS.contains(&string.as_str()) {
                            checker.diagnostics.push(Diagnostic::new(
                                LoggingExtraAttrClash(string.to_string()),
                                key.range(),
                            ));
                        }
                    }
                }
            }
        }
        ExprKind::Call(ast::ExprCall { func, keywords, .. }) => {
            if checker
                .ctx
                .resolve_call_path(func)
                .map_or(false, |call_path| call_path.as_slice() == ["", "dict"])
            {
                for keyword in keywords {
                    if let Some(key) = &keyword.node.arg {
                        if RESERVED_ATTRS.contains(&key.as_str()) {
                            checker.diagnostics.push(Diagnostic::new(
                                LoggingExtraAttrClash(key.to_string()),
                                keyword.range(),
                            ));
                        }
                    }
                }
            }
        }
        _ => {}
    }
}

#[derive(Copy, Clone)]
enum LoggingCallType {
    /// Logging call with a level method, e.g., `logging.info`.
    LevelCall(LoggingLevel),
    /// Logging call with an integer level as an argument, e.g., `logger.log(level, ...)`.
    LogCall,
}

impl LoggingCallType {
    fn from_attribute(attr: &str) -> Option<Self> {
        if attr == "log" {
            Some(LoggingCallType::LogCall)
        } else {
            LoggingLevel::from_attribute(attr).map(LoggingCallType::LevelCall)
        }
    }
}

/// Check logging calls for violations.
pub(crate) fn logging_call(
    checker: &mut Checker,
    func: &Expr,
    args: &[Expr],
    keywords: &[Keyword],
) {
    if !logging::is_logger_candidate(&checker.ctx, func) {
        return;
    }

    if let ExprKind::Attribute(ast::ExprAttribute { value, attr, .. }) = &func.node {
        if let Some(logging_call_type) = LoggingCallType::from_attribute(attr.as_str()) {
            let call_args = SimpleCallArgs::new(args, keywords);
            let level_call_range = TextRange::new(value.end() + TextSize::from(1), func.end());

            // G001 - G004
            let msg_pos = usize::from(matches!(logging_call_type, LoggingCallType::LogCall));
            if let Some(format_arg) = call_args.argument("msg", msg_pos) {
                check_msg(checker, format_arg);
            }

            // G010
            if checker.settings.rules.enabled(Rule::LoggingWarn)
                && matches!(
                    logging_call_type,
                    LoggingCallType::LevelCall(LoggingLevel::Warn)
                )
            {
                let mut diagnostic = Diagnostic::new(LoggingWarn, level_call_range);
                if checker.patch(diagnostic.kind.rule()) {
                    #[allow(deprecated)]
                    diagnostic.set_fix(Fix::unspecified(Edit::range_replacement(
                        "warning".to_string(),
                        level_call_range,
                    )));
                }
                checker.diagnostics.push(diagnostic);
            }

            // G101
            if checker.settings.rules.enabled(Rule::LoggingExtraAttrClash) {
                if let Some(extra) = find_keyword(keywords, "extra") {
                    check_log_record_attr_clash(checker, extra);
                }
            }

            // G201, G202
            if checker.settings.rules.enabled(Rule::LoggingExcInfo)
                || checker
                    .settings
                    .rules
                    .enabled(Rule::LoggingRedundantExcInfo)
            {
                if !checker.ctx.in_exception_handler() {
                    return;
                }
                if let Some(exc_info) = find_keyword(keywords, "exc_info") {
                    // If `exc_info` is `True` or `sys.exc_info()`, it's redundant; but otherwise,
                    // return.
                    if !(matches!(
                        exc_info.node.value.node,
                        ExprKind::Constant(ast::ExprConstant {
                            value: Constant::Bool(true),
                            ..
                        })
                    ) || if let ExprKind::Call(ast::ExprCall { func, .. }) =
                        &exc_info.node.value.node
                    {
                        checker
                            .ctx
                            .resolve_call_path(func)
                            .map_or(false, |call_path| {
                                call_path.as_slice() == ["sys", "exc_info"]
                            })
                    } else {
                        false
                    }) {
                        return;
                    }

                    if let LoggingCallType::LevelCall(logging_level) = logging_call_type {
                        match logging_level {
                            LoggingLevel::Error => {
                                if checker.settings.rules.enabled(Rule::LoggingExcInfo) {
                                    checker
                                        .diagnostics
                                        .push(Diagnostic::new(LoggingExcInfo, level_call_range));
                                }
                            }
                            LoggingLevel::Exception => {
                                if checker
                                    .settings
                                    .rules
                                    .enabled(Rule::LoggingRedundantExcInfo)
                                {
                                    checker.diagnostics.push(Diagnostic::new(
                                        LoggingRedundantExcInfo,
                                        exc_info.range(),
                                    ));
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
    }
}
