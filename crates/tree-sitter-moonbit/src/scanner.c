#include "tree_sitter/parser.h"
#include <stdbool.h>
#include <stdlib.h>
#include <wctype.h>

#ifndef TREE_SITTER_LANGUAGE
#define TREE_SITTER_LANGUAGE moonbit
#endif
#define tree_sitter___external_scanner(language, symbol)                       \
  tree_sitter_##language##_external_scanner_##symbol
#define tree_sitter__external_scanner(language, symbol)                        \
  tree_sitter___external_scanner(language, symbol)
#define tree_sitter_external_scanner(symbol)                                   \
  tree_sitter__external_scanner(TREE_SITTER_LANGUAGE, symbol)

#if defined(__wasi__) || defined(__EMSCRIPTEN__)
#else
#endif

#ifdef DEBUG
#include <stdio.h>
#define trace(string) printf(string)
#define tracef(format, ...) printf(format, __VA_ARGS__)
#else
#define trace(string)
#define tracef(format, ...)
#endif

struct ScannerState {
  bool remove_semi;
  bool multiline_string;
};

#if __STDC_VERSION__ >= 201112L
_Static_assert(sizeof(struct ScannerState) <=
                   TREE_SITTER_SERIALIZATION_BUFFER_SIZE,
               "Context too large");
#endif

enum TokenType {
  SCANNER_RESET,
  AUTOMATIC_SEMICOLON,
  SEMICOLON,
  MULTILINE_STRING_SEPARATOR,
  MULTILINE_INTERPOLATION_SEPARATOR,
  FLOAT_LITERAL,
  FOR_KEYWORD,
  ERROR_SENTINEL,
};

#ifdef DEBUG
static const char *const symbol_names[] = {
    [SCANNER_RESET] = "$._scanner_reset",
    [AUTOMATIC_SEMICOLON] = "$._automatic_semicolon",
    [SEMICOLON] = "\";\"",
    [MULTILINE_STRING_SEPARATOR] = "\"#|\"",
    [MULTILINE_INTERPOLATION_SEPARATOR] = "\"$|\"",
    [FLOAT_LITERAL] = "$.float_literal",
    [FOR_KEYWORD] = "\"for\"",
};
#endif

void tree_sitter_external_scanner(reset)(void *payload) {
  struct ScannerState *context = payload;
  context->remove_semi = false;
  context->multiline_string = false;
}

void *tree_sitter_external_scanner(create)(void) {
  struct ScannerState *context = malloc(sizeof(struct ScannerState));
  tree_sitter_external_scanner(reset)(context);
  return context;
}

void tree_sitter_external_scanner(destroy)(void *payload) { free(payload); }

unsigned tree_sitter_external_scanner(serialize)(void *payload, char *buffer) {
  trace("serializing\n");
  *(struct ScannerState *)buffer = *(struct ScannerState *)payload;
  return sizeof(struct ScannerState);
}

void tree_sitter_external_scanner(deserialize)(void *payload,
                                               const char *buffer,
                                               unsigned length) {
  tree_sitter_external_scanner(reset)(payload);
  if (length != sizeof(struct ScannerState)) {
    return;
  }
  *(struct ScannerState *)payload = *(struct ScannerState *)buffer;
}

static void advance(TSLexer *lexer) { lexer->advance(lexer, false); }
static void skip(TSLexer *lexer) { lexer->advance(lexer, true); }

static void skip_spaces(TSLexer *lexer, const bool *valid_symbols) {
  if (!valid_symbols[AUTOMATIC_SEMICOLON]) {
    while (iswspace(lexer->lookahead) && !lexer->eof(lexer)) {
      skip(lexer);
    }
  } else {
    while (iswblank(lexer->lookahead) && !lexer->eof(lexer)) {
      skip(lexer);
    }
  }
}

enum FloatLiteralResult {
  FLOAT_LITERAL_OK,
  FLOAT_LITERAL_NOT,
  FLOAT_LITERAL_ERR
};

static enum FloatLiteralResult
scan_decimal_float_literal_fractional_part(TSLexer *lexer) {
  if (lexer->lookahead == '.') {
    return FLOAT_LITERAL_ERR;
  }
  while (iswdigit(lexer->lookahead) || lexer->lookahead == '_') {
    advance(lexer);
  }
  if (lexer->lookahead == 'e' || lexer->lookahead == 'E') {
    advance(lexer);
    if (lexer->lookahead == '+' || lexer->lookahead == '-') {
      advance(lexer);
    }
    while (iswdigit(lexer->lookahead) || lexer->lookahead == '_') {
      advance(lexer);
    }
  }
  return FLOAT_LITERAL_OK;
}

static enum FloatLiteralResult scan_decimal_float_literal(TSLexer *lexer) {
  while (iswdigit(lexer->lookahead) || lexer->lookahead == '_') {
    advance(lexer);
  }
  if (lexer->lookahead != '.') {
    return FLOAT_LITERAL_ERR;
  }
  advance(lexer);
  return scan_decimal_float_literal_fractional_part(lexer);
}

static enum FloatLiteralResult scan_float_literal(TSLexer *lexer,
                                                  const bool *valid_symbols) {
  skip_spaces(lexer, valid_symbols);
  tracef("scan_float_literal: lookahead: %c @ %d\n", lexer->lookahead,
         lexer->get_column(lexer));
  if (!iswdigit(lexer->lookahead)) {
    return FLOAT_LITERAL_NOT;
  }
  if (lexer->lookahead == '0') {
    advance(lexer);
    if (lexer->lookahead == '.') {
      advance(lexer);
      return scan_decimal_float_literal_fractional_part(lexer);
    }
    if (iswdigit(lexer->lookahead)) {
      advance(lexer);
      return scan_decimal_float_literal(lexer);
    }
    if (lexer->lookahead != 'x' && lexer->lookahead != 'X') {
      return FLOAT_LITERAL_ERR;
    }
    advance(lexer);
    while (iswxdigit(lexer->lookahead) || lexer->lookahead == '_') {
      advance(lexer);
    }
    if (lexer->lookahead != '.') {
      return FLOAT_LITERAL_ERR;
    }
    advance(lexer);
    if (lexer->lookahead == '.') {
      return FLOAT_LITERAL_ERR;
    }
    while (iswxdigit(lexer->lookahead) || lexer->lookahead == '_') {
      advance(lexer);
    }
    if (lexer->lookahead == 'p' || lexer->lookahead == 'P') {
      advance(lexer);
      if (lexer->lookahead == '+' || lexer->lookahead == '-') {
        advance(lexer);
      }
      while (iswdigit(lexer->lookahead)) {
        advance(lexer);
      }
    }
    return FLOAT_LITERAL_OK;
  } else {
    advance(lexer);
    return scan_decimal_float_literal(lexer);
  }
}

enum AsiResult {
  ASI_REMOVE,
  ASI_INSERT,
  ASI_SKIP,
};

#ifdef DEBUG
static const char *asi_result_to_string[] = {
    [ASI_REMOVE] = "ASI_REMOVE",
    [ASI_INSERT] = "ASI_INSERT",
    [ASI_SKIP] = "ASI_SKIP",
};
#endif

static bool test_symbol_end(TSLexer *lexer) {
  if (iswalpha(lexer->lookahead) || lexer->lookahead == '_') {
    return false;
  } else {
    return true;
  }
}

static enum AsiResult asi_symbol_end(TSLexer *lexer) {
  if (iswalpha(lexer->lookahead) || lexer->lookahead == '_') {
    return ASI_INSERT;
  } else {
    return ASI_REMOVE;
  }
}

static bool test_symbol(TSLexer *lexer, const char *expected) {
  for (size_t i = 0; expected[i]; i++) {
    if (lexer->lookahead != expected[i]) {
      return false;
    }
    advance(lexer);
  }
  return test_symbol_end(lexer);
}

static enum AsiResult asi_symbol(TSLexer *lexer, const char *expected) {
  for (size_t i = 0; expected[i]; i++) {
    if (lexer->lookahead != expected[i]) {
      return ASI_INSERT;
    }
    advance(lexer);
  }
  return asi_symbol_end(lexer);
}

static enum AsiResult can_insert_semi(TSLexer *lexer,
                                      struct ScannerState *state) {
  if (state->multiline_string) {
    switch (lexer->lookahead) {
    case '#':
      advance(lexer);
      switch (lexer->lookahead) {
      case '|':
        return ASI_REMOVE;
      default:
        break;
      }
      break;
    case '$':
      advance(lexer);
      switch (lexer->lookahead) {
      case '|':
        return ASI_REMOVE;
      default:
        break;
      }
      break;
    default:
      break;
    }
  }
  switch (lexer->lookahead) {
  case '+':
    advance(lexer);
    switch (lexer->lookahead) {
    case '=':
      return ASI_REMOVE;
    default:
      return ASI_INSERT;
    }
  case '-':
    advance(lexer);
    switch (lexer->lookahead) {
    case '=':
      return ASI_REMOVE;
    default:
      return ASI_INSERT;
    }
  case '*':
    advance(lexer);
    switch (lexer->lookahead) {
    case '=':
      return ASI_REMOVE;
    default:
      return ASI_INSERT;
    }
  case '/':
    advance(lexer);
    switch (lexer->lookahead) {
    case '/':
      return ASI_SKIP;
    case '=':
      return ASI_REMOVE;
    default:
      return ASI_INSERT;
    }
  case '|':
    advance(lexer);
    switch (lexer->lookahead) {
    case '>': // PIPE
      return ASI_REMOVE;
    case '|': // BARBAR
      return ASI_INSERT;
    default: // BAR
      return ASI_REMOVE;
    }
  case '%':
    advance(lexer);
    switch (lexer->lookahead) {
    case '=':
      return ASI_REMOVE;
    default:
      return ASI_INSERT;
    }
  case '.':
    advance(lexer);
    if (lexer->lookahead == '.') {
      advance(lexer);
      if (lexer->lookahead == '.') {
        advance(lexer);
        return ASI_INSERT;
      } else {
        return ASI_REMOVE;
      }
    } else {
      return ASI_REMOVE;
    }
  case ':':
  case ',':
  case ')':
  case ']':
  case ';':
  case '=':
  case '?':
  case '!':
    return ASI_REMOVE;
  case 'a':
    advance(lexer);
    switch (lexer->lookahead) {
    case 's':
      advance(lexer);
      return asi_symbol_end(lexer);
    default:
      return ASI_INSERT;
    }
  case 'i':
    advance(lexer);
    switch (lexer->lookahead) {
    case 's':
    case 'n':
      advance(lexer);
      return asi_symbol_end(lexer);
    default:
      return ASI_INSERT;
    }
  case 'e':
    advance(lexer);
    return asi_symbol(lexer, "lse");
  case 'c':
    advance(lexer);
    return asi_symbol(lexer, "atch");
  case 'd':
    advance(lexer);
    return asi_symbol(lexer, "erive");
  case 'w':
    advance(lexer);
    return asi_symbol(lexer, "ith");
  default:
    return ASI_INSERT;
  }
}

#ifdef DEBUG
static bool trace_valid_symbols(const bool *valid_symbols) {
  for (int i = SCANNER_RESET; i < ERROR_SENTINEL; i++) {
    printf("valid_symbols[%s]: %s\n", symbol_names[i],
           valid_symbols[i] ? "true" : "false");
  }
  return false;
}
#define trace_valid_symbols(...) trace_valid_symbols(valid_symbols)
#else
#define trace_valid_symbols(...)
#endif

bool tree_sitter_external_scanner(scan)(void *payload, TSLexer *lexer,
                                        const bool *valid_symbols) {
  struct ScannerState *context = (struct ScannerState *)payload;

  trace("==========\n");
  tracef("lookahead: %c @ %d\n", lexer->lookahead, lexer->get_column(lexer));
  trace_valid_symbols(valid_symbols);

  if (lexer->eof(lexer)) {
    return false;
  }

  if (valid_symbols[ERROR_SENTINEL]) {
    return false;
  }

  if (valid_symbols[FLOAT_LITERAL]) {
    switch (scan_float_literal(lexer, valid_symbols)) {
    case FLOAT_LITERAL_OK:
      lexer->mark_end(lexer);
      lexer->result_symbol = FLOAT_LITERAL;
      trace("parsed float_literal\n");
      return true;
    case FLOAT_LITERAL_ERR:
      return false;
    case FLOAT_LITERAL_NOT:
      break;
    }
  }

  if (valid_symbols[MULTILINE_STRING_SEPARATOR] ||
      valid_symbols[MULTILINE_INTERPOLATION_SEPARATOR]) {
    skip_spaces(lexer, valid_symbols);
    tracef("multiline: lookahead: %c @ %d\n", lexer->lookahead,
           lexer->get_column(lexer));
    if (valid_symbols[MULTILINE_STRING_SEPARATOR] && lexer->lookahead == '#') {
      advance(lexer);
      if (lexer->lookahead == '|') {
        advance(lexer);
        lexer->result_symbol = MULTILINE_STRING_SEPARATOR;
        lexer->mark_end(lexer);
        context->multiline_string = true;
        tracef("setting multiline_string to %s\n", "true");
        trace("parsed multiline_string_separator\n");
        return true;
      }
    } else if (valid_symbols[MULTILINE_INTERPOLATION_SEPARATOR] &&
               lexer->lookahead == '$') {
      advance(lexer);
      if (lexer->lookahead == '|') {
        advance(lexer);
        lexer->result_symbol = MULTILINE_INTERPOLATION_SEPARATOR;
        lexer->mark_end(lexer);
        context->multiline_string = true;
        trace("parsed multiline_interpolation_separator\n");
        return true;
      }
    }
  }

  if (valid_symbols[FOR_KEYWORD]) {
    skip_spaces(lexer, valid_symbols);
    if (test_symbol(lexer, "for")) {
      lexer->result_symbol = FOR_KEYWORD;
      lexer->mark_end(lexer);
      context->remove_semi = true;
      trace("parsed for_keyword\n");
      return true;
    }
  }

  if (valid_symbols[SEMICOLON]) {
    skip_spaces(lexer, valid_symbols);
    tracef("lookahead: %c @ %d\n", lexer->lookahead, lexer->get_column(lexer));
    if (lexer->lookahead == ';') {
      advance(lexer);
      lexer->result_symbol = SEMICOLON;
      lexer->mark_end(lexer);
      context->remove_semi = true;
      trace("parsed semicolon\n");
      return true;
    }
  }

  if (valid_symbols[AUTOMATIC_SEMICOLON]) {
    while (iswblank(lexer->lookahead)) {
      skip(lexer);
    }
    tracef("lookahead: %c @ %d\n", lexer->lookahead, lexer->get_column(lexer));
    if (lexer->lookahead != '\n') {
      return false;
    }
    advance(lexer);
    lexer->result_symbol = AUTOMATIC_SEMICOLON;
    lexer->mark_end(lexer);
    tracef("lookahead: %c @ %d\n", lexer->lookahead, lexer->get_column(lexer));
    while (iswspace(lexer->lookahead)) {
      advance(lexer);
    }
    tracef("lookahead: %c @ %d\n", lexer->lookahead, lexer->get_column(lexer));
    enum AsiResult insert_semi = can_insert_semi(lexer, context);
    tracef("insert_semi: %s\n", asi_result_to_string[insert_semi]);
    tracef("remove_semi: %s\n", context->remove_semi ? "true" : "false");
    tracef("multiline  : %s\n", context->multiline_string ? "true" : "false");
    if (insert_semi == ASI_SKIP) {
      return false;
    }
    if (context->remove_semi) {
      insert_semi = ASI_REMOVE;
    }
    if (context->remove_semi || context->multiline_string) {
      tree_sitter_external_scanner(reset)(context);
      if (insert_semi == false) {
        lexer->result_symbol = SCANNER_RESET;
        trace("parsed automatic_newline\n");
        return true;
      }
    }
    return insert_semi == ASI_INSERT;
  }

  if (valid_symbols[SCANNER_RESET] && context->remove_semi) {
    lexer->result_symbol = SCANNER_RESET;
    lexer->mark_end(lexer);
    trace("parsed automatic_newline\n");
    context->remove_semi = false;
    tree_sitter_external_scanner(reset)(context);
    return true;
  }

  return false;
}
