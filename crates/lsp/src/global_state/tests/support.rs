use super::super::{AnalysisBatch, AnalysisResult, GlobalState, analyze};
use crate::test_support::MarkedProject;
use async_lsp::ClientSocket;
use lsp_types::{
    CompletionItem, CompletionParams, CompletionResponse, GotoDefinitionParams,
    GotoDefinitionResponse, InlayHint, InlayHintKind, InlayHintLabel, InlayHintParams, Location,
    PartialResultParams, Position, Range, ReferenceContext, ReferenceParams, SemanticToken,
    SemanticTokensParams, SemanticTokensRangeParams, SemanticTokensRangeResult,
    SemanticTokensResult, TextDocumentIdentifier, TextDocumentPositionParams, Url,
    WorkDoneProgressParams,
};
use snapbox::{IntoData, assert_data_eq};
use solar_config::CompileOpts;
use solar_interface::data_structures::map::FxHashSet;
use std::{
    collections::BTreeSet,
    fmt::Write as _,
    future::Future,
    io::Read as _,
    path::Path,
    sync::Arc,
    task::{Context, Poll, Waker},
};

pub(super) struct RequestFixture {
    marked: MarkedProject,
    result: AnalysisResult,
}

impl RequestFixture {
    pub(super) fn new(fixture: &str, path: &str) -> Self {
        let fixture = Self::new_allowing_diagnostics(fixture, path);
        assert!(fixture.result.diagnostics.is_empty(), "{:#?}", fixture.result.diagnostics);
        fixture
    }

    pub(super) fn new_allowing_diagnostics(fixture: &str, path: &str) -> Self {
        let marked = MarkedProject::from_fixture(fixture);
        let contents = marked.project().read_file(path);
        let path = marked.project().path(path);
        let result = analyze(AnalysisBatch {
            opts: CompileOpts::default(),
            files: vec![(path, contents)],
            seen_paths: FxHashSet::default(),
        });
        Self { marked, result }
    }

    pub(super) fn check_completion(&self, marker: &str, expected: impl IntoData) {
        let mut state = self.state();
        let (uri, position) = self.marker_location(marker);
        let response =
            expect_ready(crate::handlers::completion(&mut state, completion_params(uri, position)))
                .unwrap()
                .unwrap();
        let CompletionResponse::Array(items) = response else {
            panic!("expected completion array");
        };
        assert_data_eq!(completion_output(&items), expected);
    }

    pub(super) fn check_goto_definition(&self, marker: &str, expected: impl IntoData) {
        let mut state = self.state();
        let (uri, position) = self.marker_location(marker);
        let response =
            expect_ready(crate::handlers::goto_definition(&mut state, goto_params(uri, position)))
                .unwrap();
        assert_data_eq!(self.goto_output(response), expected);
    }

    pub(super) fn check_goto_declaration(&self, marker: &str, expected: impl IntoData) {
        let mut state = self.state();
        let (uri, position) = self.marker_location(marker);
        let response =
            expect_ready(crate::handlers::goto_declaration(&mut state, goto_params(uri, position)))
                .unwrap();
        assert_data_eq!(self.goto_output(response), expected);
    }

    pub(super) fn check_references(
        &self,
        marker: &str,
        include_declaration: bool,
        expected: impl IntoData,
    ) {
        let mut state = self.state();
        let (uri, position) = self.marker_location(marker);
        let response = expect_ready(crate::handlers::references(
            &mut state,
            reference_params(uri, position, include_declaration),
        ))
        .unwrap();
        assert_data_eq!(self.locations_output(response), expected);
    }

    pub(super) fn check_inlay_hints(&self, path: &str, expected: impl IntoData) {
        let uri = Url::from_file_path(self.marked.project().path(path)).unwrap();
        assert_data_eq!(inlay_hint_output(&self.inlay_hints(uri, full_range())), expected);
    }

    pub(super) fn check_inlay_hints_between(
        &self,
        start_marker: &str,
        end_marker: &str,
        expected: impl IntoData,
    ) {
        let (start_uri, start) = self.marker_location(start_marker);
        let (end_uri, end) = self.marker_location(end_marker);
        assert_eq!(start_uri, end_uri);
        assert_data_eq!(
            inlay_hint_output(&self.inlay_hints(start_uri, Range { start, end })),
            expected
        );
    }

    pub(super) fn semantic_tokens(&self, path: &str) -> Vec<SemanticToken> {
        let mut state = self.state();
        let uri = Url::from_file_path(self.marked.project().path(path)).unwrap();
        let response = expect_ready(crate::handlers::semantic_tokens_full(
            &mut state,
            semantic_tokens_params(uri),
        ))
        .unwrap()
        .unwrap();
        let SemanticTokensResult::Tokens(tokens) = response else {
            panic!("expected complete semantic tokens");
        };
        tokens.data
    }

    pub(super) fn check_semantic_token_summary(&self, path: &str, expected: impl IntoData) {
        let tokens = self.semantic_tokens(path);
        let contents = self.marked.project().read_file(path);
        let mut values =
            vec![BTreeSet::<String>::new(); crate::semantic_tokens::legend().token_types.len()];

        for (line, character, token) in absolute_semantic_tokens(&tokens) {
            values[token.token_type as usize].insert(utf16_text_at(
                &contents,
                line,
                character,
                token.length,
            ));
        }

        let mut output = String::new();
        for (token_type, values) in crate::semantic_tokens::legend().token_types.iter().zip(values)
        {
            write!(output, "{}", token_type.as_str().to_ascii_uppercase()).unwrap();
            for value in values {
                write!(output, " `{value}`").unwrap();
            }
            output.push('\n');
        }
        assert_data_eq!(output, expected);
    }

    pub(super) fn check_semantic_tokens(&self, path: &str, expected: impl IntoData) {
        let tokens = self.semantic_tokens(path);
        assert_data_eq!(self.semantic_token_output(path, &tokens), expected);
    }

    pub(super) fn check_semantic_tokens_in_range(
        &self,
        path: &str,
        range: Range,
        expected: impl IntoData,
    ) {
        let mut state = self.state();
        let uri = Url::from_file_path(self.marked.project().path(path)).unwrap();
        let response = expect_ready(crate::handlers::semantic_tokens_range(
            &mut state,
            semantic_tokens_range_params(uri, range),
        ))
        .unwrap()
        .unwrap();
        let SemanticTokensRangeResult::Tokens(tokens) = response else {
            panic!("expected complete semantic tokens");
        };
        assert_data_eq!(self.semantic_token_output(path, &tokens.data), expected);
    }

    fn semantic_token_output(&self, path: &str, tokens: &[SemanticToken]) -> String {
        let contents = self.marked.project().read_file(path);
        let legend = crate::semantic_tokens::legend();
        let mut output = String::new();

        for (line, character, token) in absolute_semantic_tokens(tokens) {
            let token_type = legend.token_types[token.token_type as usize].as_str();
            let text = utf16_text_at(&contents, line, character, token.length);
            writeln!(
                output,
                "{line}:{character} {} {} `{text}`",
                token.length,
                token_type.to_ascii_uppercase()
            )
            .unwrap();
        }
        output
    }

    fn inlay_hints(&self, uri: Url, range: Range) -> Vec<InlayHint> {
        let mut state = self.state();
        let response =
            expect_ready(crate::handlers::inlay_hints(&mut state, inlay_hint_params(uri, range)))
                .unwrap();
        response.unwrap_or_default()
    }

    fn state(&self) -> GlobalState {
        let mut state = GlobalState::new(ClientSocket::new_closed());
        state.config = Arc::new(self.marked.project().config());
        *state.vfs.write() = self.marked.project().vfs();
        *state.symbol_tables.write() = self.result.symbol_tables.clone();
        state
    }

    fn marker_location(&self, marker: &str) -> (Url, Position) {
        let marker = self.marked.marker(marker);
        let path = self.marked.project().path(marker.path());
        (Url::from_file_path(path).unwrap(), marker.position())
    }

    fn goto_output(&self, response: Option<GotoDefinitionResponse>) -> String {
        match response {
            Some(GotoDefinitionResponse::Array(locations)) => {
                self.locations_output(Some(locations))
            }
            Some(GotoDefinitionResponse::Scalar(location)) => {
                self.locations_output(Some(vec![location]))
            }
            Some(GotoDefinitionResponse::Link(links)) => {
                let locations = links
                    .into_iter()
                    .map(|link| Location { uri: link.target_uri, range: link.target_range })
                    .collect();
                self.locations_output(Some(locations))
            }
            None => "<none>\n".to_string(),
        }
    }

    fn locations_output(&self, response: Option<Vec<Location>>) -> String {
        let Some(locations) = response else { return "<none>\n".to_string() };
        let mut output = String::new();
        for location in locations {
            writeln!(output, "{}", self.location_output(location)).unwrap();
        }
        output
    }

    fn location_output(&self, location: Location) -> String {
        let path = location.uri.to_file_path().unwrap();
        let display_path = display_path(self.marked.project().root(), &path);
        let line = read_file(&path)
            .and_then(|contents| {
                contents.lines().nth(location.range.start.line as usize).map(str::to_owned)
            })
            .unwrap_or_default();
        format!(
            "{display_path}:{}:{} {}",
            location.range.start.line,
            location.range.start.character,
            line.trim()
        )
    }
}

fn expect_ready<F: Future>(future: F) -> F::Output {
    let waker = Waker::noop();
    let mut cx = Context::from_waker(waker);
    let mut future = std::pin::pin!(future);
    match future.as_mut().poll(&mut cx) {
        Poll::Ready(output) => output,
        Poll::Pending => panic!("request handler future should complete immediately"),
    }
}

fn absolute_semantic_tokens(
    tokens: &[SemanticToken],
) -> impl Iterator<Item = (u32, u32, &SemanticToken)> {
    let mut line = 0;
    let mut character = 0;
    tokens.iter().map(move |token| {
        line += token.delta_line;
        character =
            if token.delta_line == 0 { character + token.delta_start } else { token.delta_start };
        (line, character, token)
    })
}

fn read_file(path: &Path) -> Option<String> {
    let mut contents = String::new();
    std::fs::File::open(path).ok()?.read_to_string(&mut contents).ok()?;
    Some(contents)
}

fn utf16_text_at(contents: &str, line: u32, start: u32, length: u32) -> String {
    let line = contents.split('\n').nth(line as usize).unwrap();
    let line = line.strip_suffix('\r').unwrap_or(line);
    let end = start + length;
    let mut utf16 = 0;
    let mut start_byte = None;
    let mut end_byte = None;
    for (byte, ch) in line.char_indices() {
        if utf16 == start {
            start_byte = Some(byte);
        }
        if utf16 == end {
            end_byte = Some(byte);
            break;
        }
        utf16 += ch.len_utf16() as u32;
    }
    let start_byte = start_byte.unwrap_or(line.len());
    let end_byte = end_byte.unwrap_or(line.len());
    line[start_byte..end_byte].to_string()
}

fn completion_output(items: &[CompletionItem]) -> String {
    let mut output = String::new();
    for item in items {
        let kind = item.kind.map(|kind| format!("{kind:?}")).unwrap_or_else(|| "UNKNOWN".into());
        writeln!(output, "{} {kind}", item.label).unwrap();
    }
    output
}

fn inlay_hint_output(hints: &[InlayHint]) -> String {
    let mut output = String::new();
    for hint in hints {
        writeln!(output, "{} {}", inlay_hint_kind(hint.kind), inlay_hint_label(&hint.label))
            .unwrap();
    }
    output
}

fn inlay_hint_kind(kind: Option<InlayHintKind>) -> &'static str {
    match kind {
        Some(InlayHintKind::PARAMETER) => "PARAMETER",
        Some(InlayHintKind::TYPE) => "TYPE",
        _ => "UNKNOWN",
    }
}

fn inlay_hint_label(label: &InlayHintLabel) -> String {
    match label {
        InlayHintLabel::String(label) => label.clone(),
        InlayHintLabel::LabelParts(parts) => parts.iter().map(|part| part.value.as_str()).collect(),
    }
}

fn display_path(root: &Path, path: &Path) -> String {
    let path = path.strip_prefix(root).unwrap_or(path);
    format!("/{}", path.display())
}

fn completion_params(uri: Url, position: Position) -> CompletionParams {
    CompletionParams {
        text_document_position: text_document_position(uri, position),
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: PartialResultParams::default(),
        context: None,
    }
}

fn goto_params(uri: Url, position: Position) -> GotoDefinitionParams {
    GotoDefinitionParams {
        text_document_position_params: text_document_position(uri, position),
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: PartialResultParams::default(),
    }
}

fn reference_params(uri: Url, position: Position, include_declaration: bool) -> ReferenceParams {
    ReferenceParams {
        text_document_position: text_document_position(uri, position),
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: PartialResultParams::default(),
        context: ReferenceContext { include_declaration },
    }
}

fn inlay_hint_params(uri: Url, range: Range) -> InlayHintParams {
    InlayHintParams {
        text_document: TextDocumentIdentifier { uri },
        range,
        work_done_progress_params: WorkDoneProgressParams::default(),
    }
}

fn semantic_tokens_params(uri: Url) -> SemanticTokensParams {
    SemanticTokensParams {
        text_document: TextDocumentIdentifier { uri },
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: PartialResultParams::default(),
    }
}

fn semantic_tokens_range_params(uri: Url, range: Range) -> SemanticTokensRangeParams {
    SemanticTokensRangeParams {
        text_document: TextDocumentIdentifier { uri },
        range,
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: PartialResultParams::default(),
    }
}

fn full_range() -> Range {
    Range { start: Position::new(0, 0), end: Position::new(u32::MAX, u32::MAX) }
}

fn text_document_position(uri: Url, position: Position) -> TextDocumentPositionParams {
    TextDocumentPositionParams { text_document: TextDocumentIdentifier { uri }, position }
}
