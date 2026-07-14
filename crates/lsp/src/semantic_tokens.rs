use lsp_types::{
    Position, Range, SemanticToken, SemanticTokenType, SemanticTokens, SemanticTokensDelta,
    SemanticTokensEdit, SemanticTokensFullDeltaResult, SemanticTokensLegend, Url,
};
use solar_interface::{
    Span, Symbol,
    data_structures::{Never, map::FxHashMap},
    kw,
    source_map::SourceFile,
    sym,
};
use solar_parse::{
    Cursor,
    lexer::token::{RawLiteralKind, RawTokenKind},
    token::Delimiter,
};
use solar_sema::{
    Gcx,
    ast::{ImportItems, ItemKind, Visit as AstVisit},
    hir::{self, ContractKind, ItemId, Res, UsingEntry, UsingEntryKind, VarKind},
    ty::{CallableParamSource, ResolvedCallee, ResolvedMember, TyKind},
};
use std::{cmp::Reverse, collections::hash_map::Entry};

pub(crate) mod token_type {
    pub(crate) const NAMESPACE: u32 = 0;
    pub(crate) const TYPE: u32 = 1;
    pub(crate) const CLASS: u32 = 2;
    pub(crate) const ENUM: u32 = 3;
    pub(crate) const INTERFACE: u32 = 4;
    pub(crate) const STRUCT: u32 = 5;
    pub(crate) const PARAMETER: u32 = 6;
    pub(crate) const VARIABLE: u32 = 7;
    pub(crate) const PROPERTY: u32 = 8;
    pub(crate) const ENUM_MEMBER: u32 = 9;
    pub(crate) const EVENT: u32 = 10;
    pub(crate) const FUNCTION: u32 = 11;
    pub(crate) const METHOD: u32 = 12;
    pub(crate) const KEYWORD: u32 = 13;
    pub(crate) const COMMENT: u32 = 14;
    pub(crate) const STRING: u32 = 15;
    pub(crate) const NUMBER: u32 = 16;
    pub(crate) const OPERATOR: u32 = 17;
}

const PRIORITY_LEXICAL: u8 = 0;
const PRIORITY_SEMANTIC: u8 = 1;
const PRIORITY_AUTHORITATIVE_LEXICAL: u8 = 2;

pub(crate) fn legend() -> SemanticTokensLegend {
    SemanticTokensLegend {
        token_types: vec![
            SemanticTokenType::NAMESPACE,
            SemanticTokenType::TYPE,
            SemanticTokenType::CLASS,
            SemanticTokenType::ENUM,
            SemanticTokenType::INTERFACE,
            SemanticTokenType::STRUCT,
            SemanticTokenType::PARAMETER,
            SemanticTokenType::VARIABLE,
            SemanticTokenType::PROPERTY,
            SemanticTokenType::ENUM_MEMBER,
            SemanticTokenType::EVENT,
            SemanticTokenType::FUNCTION,
            SemanticTokenType::METHOD,
            SemanticTokenType::KEYWORD,
            SemanticTokenType::COMMENT,
            SemanticTokenType::STRING,
            SemanticTokenType::NUMBER,
            SemanticTokenType::OPERATOR,
        ],
        token_modifiers: Vec::new(),
    }
}

#[derive(Debug)]
struct CachedTokens {
    result_id: String,
    data: Vec<SemanticToken>,
}

#[derive(Debug, Default)]
pub(crate) struct SemanticTokenCache {
    next_result_id: u64,
    files: FxHashMap<Url, CachedTokens>,
    invalidation_generation: u64,
}

impl SemanticTokenCache {
    pub(crate) fn generation(&self) -> u64 {
        self.invalidation_generation
    }

    pub(crate) fn full_at_generation(
        &mut self,
        uri: Url,
        data: Vec<SemanticToken>,
        generation: u64,
        cache_result: bool,
    ) -> SemanticTokens {
        if !cache_result {
            return SemanticTokens { result_id: None, data };
        }
        if self.generation() != generation {
            return self.uncached_full(data);
        }
        let result_id = self.next_result_id();
        self.files.insert(uri, CachedTokens { result_id: result_id.clone(), data: data.clone() });
        SemanticTokens { result_id: Some(result_id), data }
    }

    pub(crate) fn delta_at_generation(
        &mut self,
        uri: Url,
        previous_result_id: &str,
        data: Vec<SemanticToken>,
        generation: u64,
    ) -> SemanticTokensFullDeltaResult {
        if self.generation() != generation {
            return self.uncached_full(data).into();
        }
        let edits = self
            .files
            .get(&uri)
            .filter(|cached| cached.result_id == previous_result_id)
            .map(|cached| token_delta(&cached.data, &data));
        let result_id = self.next_result_id();

        match edits {
            Some(edits) => {
                self.files.insert(uri, CachedTokens { result_id: result_id.clone(), data });
                SemanticTokensDelta { result_id: Some(result_id), edits }.into()
            }
            None => {
                self.files
                    .insert(uri, CachedTokens { result_id: result_id.clone(), data: data.clone() });
                SemanticTokens { result_id: Some(result_id), data }.into()
            }
        }
    }

    pub(crate) fn remove(&mut self, uri: &Url) {
        self.files.remove(uri);
        self.invalidation_generation += 1;
    }

    #[cfg(test)]
    pub(crate) fn contains(&self, uri: &Url) -> bool {
        self.files.contains_key(uri)
    }

    fn next_result_id(&mut self) -> String {
        self.next_result_id += 1;
        self.next_result_id.to_string()
    }

    fn uncached_full(&mut self, data: Vec<SemanticToken>) -> SemanticTokens {
        SemanticTokens { result_id: Some(self.next_result_id()), data }
    }
}

fn token_delta(old: &[SemanticToken], new: &[SemanticToken]) -> Vec<SemanticTokensEdit> {
    let prefix = old.iter().zip(new).take_while(|(old, new)| old == new).count();
    if prefix == old.len() && prefix == new.len() {
        return Vec::new();
    }

    let suffix = old[prefix..]
        .iter()
        .rev()
        .zip(new[prefix..].iter().rev())
        .take_while(|(old, new)| old == new)
        .count();
    let inserted = &new[prefix..new.len() - suffix];
    vec![SemanticTokensEdit {
        start: (prefix * 5) as u32,
        delete_count: ((old.len() - prefix - suffix) * 5) as u32,
        data: (!inserted.is_empty()).then(|| inserted.to_vec()),
    }]
}

#[derive(Clone, Copy, Debug)]
struct AbsoluteToken {
    range: Range,
    token_type: u32,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct SemanticTokenIndex {
    /// Tokens are sorted by range and do not overlap within each file.
    files: FxHashMap<Url, Vec<AbsoluteToken>>,
}

impl SemanticTokenIndex {
    pub(crate) fn extend(&mut self, other: Self) {
        for (uri, tokens) in other.files {
            match self.files.entry(uri) {
                Entry::Occupied(mut entry) => {
                    let current = entry.get_mut();
                    current.extend(tokens);
                    normalize_absolute_tokens(current);
                }
                Entry::Vacant(entry) => {
                    entry.insert(tokens);
                }
            }
        }
    }

    pub(crate) fn tokens(&self, uri: &Url, range: Option<Range>) -> Vec<SemanticToken> {
        let tokens = self.files.get(uri).map(Vec::as_slice).unwrap_or_default();
        let tokens = match range {
            None => tokens,
            Some(range) if range.start < range.end => {
                let start = tokens.partition_point(|token| token.range.end <= range.start);
                let end =
                    start + tokens[start..].partition_point(|token| token.range.start < range.end);
                &tokens[start..end]
            }
            Some(_) => &[],
        };
        encode(tokens)
    }
}

#[derive(Clone, Copy, Debug)]
struct BuilderToken {
    span: Span,
    token_type: u32,
    priority: u8,
}

pub(crate) struct SemanticTokenBuilder<'gcx> {
    gcx: Gcx<'gcx>,
    tokens: Vec<BuilderToken>,
    enabled: bool,
}

impl<'gcx> SemanticTokenBuilder<'gcx> {
    pub(crate) fn new(gcx: Gcx<'gcx>, enabled: bool) -> Self {
        let mut this = Self { gcx, tokens: Vec::new(), enabled };
        if enabled {
            this.collect_lexical_tokens();
            this.collect_imports();
            this.collect_overrides();
        }
        this
    }

    pub(crate) fn push_declaration(&mut self, item_id: ItemId, span: Span) {
        if !self.enabled {
            return;
        }
        self.push_semantic(span, item_token_type(self.gcx, item_id));
    }

    pub(crate) fn push_enum_variant(&mut self, span: Span) {
        self.push_semantic(span, token_type::ENUM_MEMBER);
    }

    pub(crate) fn push_modifier(&mut self, item_id: ItemId, span: Span) {
        if !self.enabled {
            return;
        }
        self.push_last_ident(span, item_token_type(self.gcx, item_id));
    }

    pub(crate) fn push_using_entry(&mut self, entry: &UsingEntry<'gcx>) {
        if !self.enabled {
            return;
        }
        let token_type = match entry.kind {
            UsingEntryKind::Library(_) => Some(token_type::NAMESPACE),
            UsingEntryKind::Functions(functions) => unanimous(
                functions.iter().map(|&id| item_token_type(self.gcx, ItemId::Function(id))),
            ),
            UsingEntryKind::Err(_) => None,
        };
        if let Some(token_type) = token_type {
            self.push_last_ident(entry.span, token_type);
        }
    }

    pub(crate) fn push_ident_expr(
        &mut self,
        expr: &hir::Expr<'gcx>,
        resolutions: &[Res],
        callee: Option<ResolvedCallee>,
    ) {
        if !self.enabled {
            return;
        }
        let token_type = if let Some(callee) = callee {
            self.res_token_type(callee.res, expr, false)
        } else {
            unanimous(resolutions.iter().filter_map(|&res| self.res_token_type(res, expr, false)))
        };
        if let Some(token_type) = token_type {
            self.push_semantic(expr.span, token_type);
        }
    }

    pub(crate) fn push_member_expr(
        &mut self,
        expr: &hir::Expr<'gcx>,
        member: solar_interface::Ident,
        is_yul: bool,
        resolved: Option<ResolvedMember>,
        callee: Option<ResolvedCallee>,
    ) {
        if !self.enabled {
            return;
        }
        let token_type = resolved
            .and_then(|resolved| self.resolved_member_token_type(resolved, expr))
            .or_else(|| callee.and_then(|callee| self.res_token_type(callee.res, expr, true)))
            .or(is_yul.then_some(token_type::PROPERTY));
        if let Some(token_type) = token_type {
            self.push_semantic(member.span, token_type);
        }
    }

    pub(crate) fn push_custom_type(&mut self, item_id: ItemId, span: Span) {
        if !self.enabled {
            return;
        }
        self.push_last_ident(span, item_token_type(self.gcx, item_id));
    }

    pub(crate) fn push_call_names(
        &mut self,
        callee: &hir::Expr<'gcx>,
        args: &hir::CallArgs<'gcx>,
        options: Option<&hir::CallOptions<'gcx>>,
    ) {
        if !self.enabled {
            return;
        }

        if let Some(options) = options {
            for option in options.args {
                if matches!(option.name.name, kw::Gas | sym::value | sym::salt) {
                    self.push_semantic(option.name.span, token_type::PROPERTY);
                }
            }
        }

        let hir::CallArgsKind::Named(args) = args.kind else { return };
        let Some(source) = self
            .gcx
            .type_of_expr(callee.id)
            .and_then(|ty| self.gcx.callable_signature_of_ty(ty))
            .and_then(|signature| signature.param_source)
        else {
            return;
        };
        let names = self.gcx.callable_param_names(source);
        let token_type = if matches!(source, CallableParamSource::Struct(_)) {
            token_type::PROPERTY
        } else {
            token_type::PARAMETER
        };
        for arg in args {
            if names.iter().flatten().any(|&name| name == arg.name.name) {
                self.push_semantic(arg.name.span, token_type);
            }
        }
    }

    pub(crate) fn push_mapping_names(&mut self, mapping: &hir::TypeMapping<'gcx>) {
        if !self.enabled {
            return;
        }
        for name in [mapping.key_name, mapping.value_name].into_iter().flatten() {
            self.push_semantic(name.span, token_type::PARAMETER);
        }
    }

    pub(crate) fn finish(mut self) -> SemanticTokenIndex {
        if !self.enabled {
            return SemanticTokenIndex::default();
        }
        self.tokens.sort_unstable_by_key(|token| {
            (token.span.lo(), Reverse(token.priority), token.span.hi(), token.token_type)
        });

        let mut previous_hi = None;
        self.tokens.retain(|token| {
            if previous_hi.is_some_and(|hi| token.span.lo() < hi) {
                return false;
            }
            previous_hi = Some(token.span.hi());
            true
        });

        let source_map = self.gcx.sess.source_map();
        let files = source_map.files();
        let mut index = SemanticTokenIndex::default();
        let mut tokens = self.tokens.into_iter().peekable();
        // Both inputs are ordered by absolute byte position, so consume each file in one pass.
        for file in files.iter() {
            let file_end = file.end_position();
            while tokens.peek().is_some_and(|token| token.span.lo() < file.start_pos) {
                tokens.next();
            }
            if tokens.peek().is_none_or(|token| token.span.lo() > file_end) {
                continue;
            }

            let mut output = file
                .name
                .as_real()
                .and_then(|path| Url::from_file_path(path).ok())
                .map(|uri| index.files.entry(uri).or_default());
            let mut positions = PositionEncoder::new(file);
            while tokens.peek().is_some_and(|token| token.span.lo() <= file_end) {
                let token = tokens.next().unwrap();
                if token.span.hi() <= file_end
                    && let Some(output) = &mut output
                {
                    push_span_segments(output, file, token, &mut positions);
                }
            }
        }
        index
    }

    fn collect_lexical_tokens(&mut self) {
        for source in self.gcx.sources.iter() {
            let file = &source.file;
            let mut yul_depth = 0;
            let mut assembly_pending = false;
            for (start, token) in Cursor::new(&file.src).with_position() {
                let text = &file.src[start..start + token.len as usize];
                let in_yul = yul_depth > 0;
                let symbol =
                    (token.kind == RawTokenKind::Ident).then(|| self.gcx.sess.intern(text));
                if let Some((token_type, priority)) =
                    Self::lexical_token_type(token.kind, text, in_yul, symbol)
                {
                    let lo = file.start_pos + start as u32;
                    self.push(Span::new(lo, lo + token.len), token_type, priority);
                }
                Self::update_yul_context(token.kind, symbol, &mut yul_depth, &mut assembly_pending);
            }
        }
    }

    fn update_yul_context(
        kind: RawTokenKind,
        symbol: Option<Symbol>,
        yul_depth: &mut usize,
        assembly_pending: &mut bool,
    ) {
        if symbol == Some(kw::Assembly) {
            *assembly_pending = true;
            return;
        }

        match kind {
            RawTokenKind::OpenDelim(Delimiter::Brace) if *assembly_pending => {
                *assembly_pending = false;
                *yul_depth = 1;
            }
            RawTokenKind::OpenDelim(_) if *yul_depth > 0 => *yul_depth += 1,
            RawTokenKind::CloseDelim(_) if *yul_depth > 0 => *yul_depth -= 1,
            RawTokenKind::Semi => *assembly_pending = false,
            RawTokenKind::CloseDelim(Delimiter::Brace) => *assembly_pending = false,
            _ => {}
        }
    }

    fn collect_imports(&mut self) {
        for source_id in self.gcx.hir.source_ids() {
            let hir_source = self.gcx.hir.source(source_id);
            let Some((_, parsed_source)) = self.gcx.sources.get_file(&hir_source.file) else {
                continue;
            };
            let Some(ast) = &parsed_source.ast else { continue };

            for (item_id, item) in ast.items.iter_enumerated() {
                let ItemKind::Import(import) = &item.kind else { continue };

                match &import.items {
                    ImportItems::Plain(Some(alias)) | ImportItems::Glob(alias) => {
                        self.push_semantic(alias.span, token_type::NAMESPACE);
                    }
                    ImportItems::Plain(None) => {}
                    ImportItems::Aliases(imports) => {
                        let Some(imported_source) =
                            hir_source.imports.iter().find_map(|&(import_id, source_id)| {
                                (import_id == item_id).then_some(source_id)
                            })
                        else {
                            continue;
                        };
                        for (name, alias) in imports.iter() {
                            let Some(token_type) =
                                self.imported_item_token_type(imported_source, name.name)
                            else {
                                continue;
                            };
                            self.push_semantic(name.span, token_type);
                            if let Some(alias) = alias {
                                self.push_semantic(alias.span, token_type);
                            }
                        }
                    }
                }
            }
        }
    }

    fn collect_overrides(&mut self) {
        let mut spans = Vec::new();
        for ast in self.gcx.sources.asts() {
            let mut collector = OverrideCollector { spans: &mut spans };
            let _ = collector.visit_source_unit(ast);
        }
        for span in spans {
            self.push_semantic(span, token_type::CLASS);
        }
    }

    fn imported_item_token_type(
        &self,
        source_id: hir::SourceId,
        name: solar_interface::Symbol,
    ) -> Option<u32> {
        unanimous(self.gcx.hir.source(source_id).items.iter().filter_map(|&item_id| {
            if matches!(item_id, ItemId::Function(id) if self.gcx.hir.function(id).is_yul) {
                return None;
            }
            let item = self.gcx.hir.item(item_id);
            (item.name()?.name == name).then(|| item_token_type(self.gcx, item_id))
        }))
    }

    fn lexical_token_type(
        kind: RawTokenKind,
        text: &str,
        in_yul: bool,
        symbol: Option<Symbol>,
    ) -> Option<(u32, u8)> {
        let token = match kind {
            RawTokenKind::LineComment { .. } | RawTokenKind::BlockComment { .. } => {
                (token_type::COMMENT, PRIORITY_AUTHORITATIVE_LEXICAL)
            }
            RawTokenKind::Literal { kind: RawLiteralKind::Str { .. } } => {
                (token_type::STRING, PRIORITY_AUTHORITATIVE_LEXICAL)
            }
            RawTokenKind::Literal {
                kind: RawLiteralKind::Int { .. } | RawLiteralKind::Rational { .. },
            } => (token_type::NUMBER, PRIORITY_LEXICAL),
            RawTokenKind::Ident => {
                let symbol = symbol?;
                if symbol.is_elementary_type() || is_sized_fixed_type(text) {
                    (token_type::TYPE, PRIORITY_LEXICAL)
                } else if symbol.is_used_keyword()
                    || symbol.is_unused_keyword()
                    || symbol.is_bool_lit()
                    || is_contextual_keyword(symbol)
                    || (in_yul && (symbol.is_yul_keyword() || symbol.is_yul_builtin()))
                {
                    (token_type::KEYWORD, PRIORITY_LEXICAL)
                } else {
                    return None;
                }
            }
            kind if is_operator(kind) => (token_type::OPERATOR, PRIORITY_LEXICAL),
            _ => return None,
        };
        Some(token)
    }

    fn res_token_type(&self, res: Res, expr: &hir::Expr<'gcx>, is_member: bool) -> Option<u32> {
        match res {
            Res::Item(item_id) => Some(item_token_type(self.gcx, item_id)),
            Res::Namespace(_) => Some(token_type::NAMESPACE),
            Res::Builtin(builtin) => self
                .gcx
                .type_of_expr(expr.id)
                .and_then(|ty| match ty.kind {
                    TyKind::Fn(_) => {
                        Some(if is_member { token_type::METHOD } else { token_type::FUNCTION })
                    }
                    TyKind::Module(_) | TyKind::BuiltinModule(_) => Some(token_type::NAMESPACE),
                    TyKind::Contract(_) | TyKind::Super(_) => Some(token_type::CLASS),
                    _ if is_member => Some(token_type::PROPERTY),
                    _ => None,
                })
                .or_else(|| builtin.members().is_some().then_some(token_type::NAMESPACE)),
            Res::Err(_) => None,
        }
    }

    fn resolved_member_token_type(
        &self,
        member: ResolvedMember,
        expr: &hir::Expr<'gcx>,
    ) -> Option<u32> {
        match member {
            ResolvedMember::Res(res) => self.res_token_type(res, expr, true),
            ResolvedMember::StructField { .. } => Some(token_type::PROPERTY),
            ResolvedMember::EnumVariant { .. } => Some(token_type::ENUM_MEMBER),
        }
    }

    fn push_last_ident(&mut self, span: Span, token_type: u32) {
        let source_map = self.gcx.sess.source_map();
        let Ok(source) = source_map.span_to_source(span) else { return };
        let text = &source.file.src[source.data.clone()];
        let Some((start, token)) = Cursor::new(text)
            .with_position()
            .filter(|(_, token)| token.kind == RawTokenKind::Ident)
            .last()
        else {
            return;
        };
        let lo = span.lo() + start as u32;
        self.push_semantic(Span::new(lo, lo + token.len), token_type);
    }

    fn push_semantic(&mut self, span: Span, token_type: u32) {
        self.push(span, token_type, PRIORITY_SEMANTIC);
    }

    fn push(&mut self, span: Span, token_type: u32, priority: u8) {
        if self.enabled && !span.is_dummy() && span.lo() < span.hi() {
            self.tokens.push(BuilderToken { span, token_type, priority });
        }
    }
}

fn item_token_type(gcx: Gcx<'_>, item_id: ItemId) -> u32 {
    match item_id {
        ItemId::Contract(id) => match gcx.hir.contract(id).kind {
            ContractKind::Contract | ContractKind::AbstractContract => token_type::CLASS,
            ContractKind::Interface => token_type::INTERFACE,
            ContractKind::Library => token_type::NAMESPACE,
        },
        ItemId::Function(id) => {
            let function = gcx.hir.function(id);
            if function.is_yul || function.contract.is_none() {
                token_type::FUNCTION
            } else {
                token_type::METHOD
            }
        }
        ItemId::Variable(id) => match gcx.hir.variable(id).kind {
            VarKind::State | VarKind::Struct => token_type::PROPERTY,
            VarKind::Event
            | VarKind::Error
            | VarKind::FunctionParam
            | VarKind::FunctionReturn
            | VarKind::FunctionTyParam
            | VarKind::FunctionTyReturn
            | VarKind::TryCatch => token_type::PARAMETER,
            VarKind::Global | VarKind::Statement => token_type::VARIABLE,
        },
        ItemId::Struct(_) => token_type::STRUCT,
        ItemId::Enum(_) => token_type::ENUM,
        ItemId::Udvt(_) | ItemId::Error(_) => token_type::TYPE,
        ItemId::Event(_) => token_type::EVENT,
    }
}

struct PositionEncoder<'a> {
    source: &'a str,
    ascii: bool,
    line: usize,
    byte: usize,
    character: u32,
}

impl<'a> PositionEncoder<'a> {
    fn new(file: &'a SourceFile) -> Self {
        Self {
            source: &file.src,
            ascii: file.multibyte_chars.is_empty(),
            line: 0,
            byte: 0,
            character: 0,
        }
    }

    fn columns(
        &mut self,
        line: usize,
        line_start: usize,
        segment_start: usize,
        segment_end: usize,
    ) -> (u32, u32) {
        if self.ascii {
            let start = (segment_start - line_start) as u32;
            return (start, start + (segment_end - segment_start) as u32);
        }

        if self.line != line {
            self.line = line;
            self.byte = line_start;
            self.character = 0;
        }
        debug_assert!(self.byte <= segment_start);
        self.character += self.source[self.byte..segment_start].encode_utf16().count() as u32;
        let start = self.character;
        self.character += self.source[segment_start..segment_end].encode_utf16().count() as u32;
        self.byte = segment_end;
        (start, self.character)
    }
}

struct OverrideCollector<'a> {
    spans: &'a mut Vec<Span>,
}

impl<'ast> AstVisit<'ast> for OverrideCollector<'_> {
    type BreakValue = Never;

    fn visit_override(
        &mut self,
        override_: &'ast solar_sema::ast::Override<'ast>,
    ) -> std::ops::ControlFlow<Self::BreakValue> {
        self.spans.extend(
            override_
                .paths
                .iter()
                .filter_map(|path| path.segments().last().map(|ident| ident.span)),
        );
        self.walk_override(override_)
    }
}

fn push_span_segments(
    tokens: &mut Vec<AbsoluteToken>,
    file: &SourceFile,
    token: BuilderToken,
    positions: &mut PositionEncoder<'_>,
) {
    let start = file.relative_position(token.span.lo());
    let end = file.relative_position(token.span.hi());
    let Some(start_line) = file.lookup_line(start) else { return };
    let Some(end_line) = file.lookup_line(end) else { return };
    let bytes = file.src.as_bytes();
    let start = start.to_usize();
    let end = end.to_usize();

    for line in start_line..=end_line {
        let Some(line_start) = file.line_position(line) else { continue };
        let mut line_end = file.line_position(line + 1).unwrap_or(file.src.len());
        if line_end > line_start && bytes[line_end - 1] == b'\n' {
            line_end -= 1;
        }
        if line_end > line_start && bytes[line_end - 1] == b'\r' {
            line_end -= 1;
        }
        let segment_start = start.max(line_start);
        let segment_end = end.min(line_end);
        if segment_start >= segment_end {
            continue;
        }

        let (character, end_character) =
            positions.columns(line, line_start, segment_start, segment_end);
        let length = end_character - character;
        if length == 0 {
            continue;
        }
        tokens.push(AbsoluteToken {
            range: Range {
                start: Position { line: line as u32, character },
                end: Position { line: line as u32, character: character + length },
            },
            token_type: token.token_type,
        });
    }
}

fn normalize_absolute_tokens(tokens: &mut Vec<AbsoluteToken>) {
    debug_assert!(tokens.iter().all(|token| token.range.start < token.range.end));
    tokens.sort_unstable_by_key(|token| (token.range.start, token.range.end, token.token_type));

    let mut write = 0;
    for read in 0..tokens.len() {
        if write > 0 && tokens[read].range.start < tokens[write - 1].range.end {
            continue;
        }
        tokens[write] = tokens[read];
        write += 1;
    }
    tokens.truncate(write);
}

fn encode(tokens: &[AbsoluteToken]) -> Vec<SemanticToken> {
    let mut previous = Position::new(0, 0);
    let mut encoded = Vec::with_capacity(tokens.len());
    for token in tokens.iter().copied() {
        let delta_line = token.range.start.line - previous.line;
        let delta_start = if delta_line == 0 {
            token.range.start.character - previous.character
        } else {
            token.range.start.character
        };
        previous = token.range.start;
        encoded.push(SemanticToken {
            delta_line,
            delta_start,
            length: token.range.end.character - token.range.start.character,
            token_type: token.token_type,
            token_modifiers_bitset: 0,
        });
    }
    encoded
}

fn unanimous(mut values: impl Iterator<Item = u32>) -> Option<u32> {
    let first = values.next()?;
    values.all(|value| value == first).then_some(first)
}

fn is_operator(kind: RawTokenKind) -> bool {
    matches!(
        kind,
        RawTokenKind::Eq
            | RawTokenKind::Lt
            | RawTokenKind::Le
            | RawTokenKind::EqEq
            | RawTokenKind::Ne
            | RawTokenKind::Ge
            | RawTokenKind::Gt
            | RawTokenKind::AndAnd
            | RawTokenKind::OrOr
            | RawTokenKind::Not
            | RawTokenKind::Tilde
            | RawTokenKind::Walrus
            | RawTokenKind::PlusPlus
            | RawTokenKind::MinusMinus
            | RawTokenKind::StarStar
            | RawTokenKind::BinOp(_)
            | RawTokenKind::BinOpEq(_)
            | RawTokenKind::Colon
            | RawTokenKind::Arrow
            | RawTokenKind::FatArrow
            | RawTokenKind::Question
    )
}

fn is_sized_fixed_type(text: &str) -> bool {
    let Some((m, n)) = text
        .strip_prefix("fixed")
        .or_else(|| text.strip_prefix("ufixed"))
        .and_then(|size| size.split_once('x'))
    else {
        return false;
    };
    valid_integer_size(m) && n.parse::<u8>().is_ok_and(|n| n <= 80)
}

fn valid_integer_size(size: &str) -> bool {
    size.parse::<u16>().is_ok_and(|size| (8..=256).contains(&size) && size % 8 == 0)
}

fn is_contextual_keyword(symbol: solar_interface::Symbol) -> bool {
    matches!(
        symbol,
        sym::at
            | sym::code
            | sym::data
            | sym::error
            | sym::from
            | sym::global
            | sym::layout
            | sym::object
            | sym::solidity
            | sym::transient
            | sym::underscore
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use lsp_types::SemanticTokensFullDeltaResult;

    #[test]
    fn token_delta_uses_flat_array_offsets() {
        let old = [token(1), token(2), token(3)];
        let replacement = token(4);
        let new = [token(1), replacement, token(3)];

        assert_eq!(
            token_delta(&old, &new),
            vec![lsp_types::SemanticTokensEdit {
                start: 5,
                delete_count: 5,
                data: Some(vec![replacement]),
            }]
        );
        assert!(token_delta(&old, &old).is_empty());
    }

    #[test]
    fn range_tokens_use_half_open_overlap_boundaries() {
        let uri = Url::parse("file:///tokens.sol").unwrap();
        let mut index = SemanticTokenIndex::default();
        index.files.insert(
            uri.clone(),
            vec![
                absolute_token(1, 3, token_type::TYPE),
                absolute_token(5, 7, token_type::PROPERTY),
            ],
        );

        let range =
            |start, end| Some(Range { start: Position::new(0, start), end: Position::new(0, end) });
        assert!(index.tokens(&uri, range(0, 1)).is_empty());
        assert!(index.tokens(&uri, range(3, 5)).is_empty());
        assert_eq!(
            index.tokens(&uri, range(2, 6)),
            vec![
                SemanticToken {
                    delta_line: 0,
                    delta_start: 1,
                    length: 2,
                    token_type: token_type::TYPE,
                    token_modifiers_bitset: 0,
                },
                SemanticToken {
                    delta_line: 0,
                    delta_start: 4,
                    length: 2,
                    token_type: token_type::PROPERTY,
                    token_modifiers_bitset: 0,
                },
            ]
        );
    }

    #[test]
    fn cache_returns_deltas_only_for_the_latest_matching_result() {
        let uri = Url::parse("file:///tokens.sol").unwrap();
        let mut cache = SemanticTokenCache::default();
        let generation = cache.generation();
        let first = cache.full_at_generation(
            uri.clone(),
            vec![token(1), token(2), token(3)],
            generation,
            true,
        );
        assert_eq!(first.result_id.as_deref(), Some("1"));

        let generation = cache.generation();
        let delta = cache.delta_at_generation(
            uri.clone(),
            "1",
            vec![token(1), token(4), token(3)],
            generation,
        );
        let SemanticTokensFullDeltaResult::TokensDelta(delta) = delta else {
            panic!("expected token delta");
        };
        assert_eq!(delta.result_id.as_deref(), Some("2"));
        assert_eq!(delta.edits[0].start, 5);

        let generation = cache.generation();
        let full = cache.delta_at_generation(uri.clone(), "1", vec![token(1)], generation);
        let SemanticTokensFullDeltaResult::Tokens(full) = full else {
            panic!("expected full tokens for a stale result id");
        };
        assert_eq!(full.result_id.as_deref(), Some("3"));

        cache.remove(&uri);
        assert!(!cache.contains(&uri));

        let uncached = cache.full_at_generation(uri.clone(), vec![token(5)], 0, true);
        assert!(!cache.contains(&uri));
        assert!(uncached.result_id.is_some());
    }

    #[test]
    fn cache_skips_history_without_delta_support() {
        let uri = Url::parse("file:///tokens.sol").unwrap();
        let mut cache = SemanticTokenCache::default();
        let generation = cache.generation();

        let full = cache.full_at_generation(uri.clone(), vec![token(1)], generation, false);

        assert_eq!(full.result_id, None);
        assert!(!cache.contains(&uri));
    }

    #[test]
    fn recognizes_only_valid_sized_fixed_types() {
        for text in ["fixed8x0", "ufixed256x80"] {
            assert!(is_sized_fixed_type(text), "expected valid type: {text}");
        }
        for text in ["fixed", "ufixed", "fixed7x1", "fixed8x81"] {
            assert!(!is_sized_fixed_type(text), "unexpected type: {text}");
        }
    }

    fn token(value: u32) -> SemanticToken {
        SemanticToken {
            delta_line: value,
            delta_start: value,
            length: value,
            token_type: value,
            token_modifiers_bitset: value,
        }
    }

    fn absolute_token(start: u32, end: u32, token_type: u32) -> AbsoluteToken {
        AbsoluteToken {
            range: Range { start: Position::new(0, start), end: Position::new(0, end) },
            token_type,
        }
    }
}
