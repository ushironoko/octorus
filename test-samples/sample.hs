{- |
Haskell sample file for tree-sitter syntax highlighting test
Phase 1 language support
-}

{-# LANGUAGE GADTs #-}
{-# LANGUAGE TypeFamilies #-}
{-# LANGUAGE OverloadedStrings #-}

module TreeSitter.Highlighter
    ( Token(..)
    , Scope(..)
    , highlight
    , parseCode
    , SyntaxTree
    ) where

import Data.Text (Text)
import qualified Data.Text as T
import Data.Map.Strict (Map)
import qualified Data.Map.Strict as Map
import Control.Monad (forM_, when)
import Data.Maybe (fromMaybe, mapMaybe)

-- | Represents different token scopes for highlighting
data Scope
    = Keyword
    | Function
    | TypeName
    | StringLiteral
    | NumberLiteral
    | Comment
    | Operator
    | Punctuation
    deriving (Show, Eq, Ord)

-- | A token with position and scope information
data Token = Token
    { tokenStart  :: !Int
    , tokenEnd    :: !Int
    , tokenScope  :: !Scope
    , tokenText   :: !Text
    } deriving (Show, Eq)

-- | Type alias for syntax tree representation
type SyntaxTree = Map Int [Token]

-- | GADT for typed expressions
data Expr a where
    LitInt    :: Int -> Expr Int
    LitString :: Text -> Expr Text
    LitBool   :: Bool -> Expr Bool
    Add       :: Expr Int -> Expr Int -> Expr Int
    Concat    :: Expr Text -> Expr Text -> Expr Text
    IfThenElse :: Expr Bool -> Expr a -> Expr a -> Expr a

-- | Evaluate a typed expression
eval :: Expr a -> a
eval (LitInt n)        = n
eval (LitString s)     = s
eval (LitBool b)       = b
eval (Add e1 e2)       = eval e1 + eval e2
eval (Concat e1 e2)    = eval e1 <> eval e2
eval (IfThenElse c t e) = if eval c then eval t else eval e

-- | Type class for highlightable languages
class Highlightable a where
    type Query a
    getLanguage :: a -> Text
    buildQuery :: a -> Query a
    runQuery :: a -> Query a -> Text -> [Token]

-- | Haskell language highlighter
data HaskellHighlighter = HaskellHighlighter
    { hlKeywords :: [Text]
    , hlOperators :: [Text]
    }

-- | Default Haskell highlighter instance
defaultHighlighter :: HaskellHighlighter
defaultHighlighter = HaskellHighlighter
    { hlKeywords = ["module", "where", "import", "data", "newtype", "type"
                   , "class", "instance", "deriving", "if", "then", "else"
                   , "case", "of", "let", "in", "do", "forall"]
    , hlOperators = ["->", "<-", "=>", "::", "=", "|", "\\", "@", "~"]
    }

-- | Highlight a piece of code
highlight :: HaskellHighlighter -> Text -> SyntaxTree
highlight hl code = buildTree $ tokenize hl code

-- | Tokenize source code
tokenize :: HaskellHighlighter -> Text -> [Token]
tokenize hl code = concatMap (tokenizeLine hl) (zip [0..] (T.lines code))

-- | Tokenize a single line
tokenizeLine :: HaskellHighlighter -> (Int, Text) -> [Token]
tokenizeLine hl (lineNum, line) =
    mapMaybe (makeToken lineNum) (T.words line)
  where
    makeToken :: Int -> Text -> Maybe Token
    makeToken ln word
        | word `elem` hlKeywords hl = Just $ Token
            { tokenStart = ln * 100  -- Simplified position
            , tokenEnd = ln * 100 + T.length word
            , tokenScope = Keyword
            , tokenText = word
            }
        | word `elem` hlOperators hl = Just $ Token
            { tokenStart = ln * 100
            , tokenEnd = ln * 100 + T.length word
            , tokenScope = Operator
            , tokenText = word
            }
        | T.all (`elem` ['0'..'9']) word = Just $ Token
            { tokenStart = ln * 100
            , tokenEnd = ln * 100 + T.length word
            , tokenScope = NumberLiteral
            , tokenText = word
            }
        | otherwise = Nothing

-- | Build a syntax tree from tokens
buildTree :: [Token] -> SyntaxTree
buildTree = foldr insertToken Map.empty
  where
    insertToken :: Token -> SyntaxTree -> SyntaxTree
    insertToken tok = Map.insertWith (++) (tokenStart tok `div` 100) [tok]

-- | Parse code and return structured result
parseCode :: Text -> Either Text SyntaxTree
parseCode code
    | T.null code = Left "Empty code"
    | otherwise   = Right $ highlight defaultHighlighter code

-- | Monadic example with do notation
processFiles :: [FilePath] -> IO ()
processFiles files = do
    putStrLn "Processing files..."
    forM_ files $ \file -> do
        content <- readFile file
        let tree = highlight defaultHighlighter (T.pack content)
        putStrLn $ "Processed: " ++ file
        when (Map.size tree > 0) $
            putStrLn $ "  Found " ++ show (Map.size tree) ++ " lines with tokens"

-- | Pattern matching with guards
categorizeToken :: Token -> String
categorizeToken tok
    | tokenScope tok == Keyword    = "Reserved word"
    | tokenScope tok == Function   = "Function name"
    | tokenScope tok == TypeName   = "Type identifier"
    | tokenScope tok `elem` [StringLiteral, NumberLiteral] = "Literal value"
    | otherwise = "Other"

-- | List comprehension example
allKeywordTokens :: SyntaxTree -> [Token]
allKeywordTokens tree =
    [ tok
    | tokens <- Map.elems tree
    , tok <- tokens
    , tokenScope tok == Keyword
    ]

-- | Lambda and higher-order function example
applyToTokens :: (Token -> a) -> SyntaxTree -> [[a]]
applyToTokens f = map (map f) . Map.elems

-- | Point-free style
countTokens :: SyntaxTree -> Int
countTokens = sum . map length . Map.elems
