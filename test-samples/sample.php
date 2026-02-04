<?php
/**
 * PHP sample file for tree-sitter syntax highlighting test
 * Phase 1 language support
 */

declare(strict_types=1);

namespace App\TreeSitter;

use InvalidArgumentException;
use RuntimeException;

// Interface definition
interface LanguageParser
{
    public function parse(string $code): array;
    public function getLanguageName(): string;
}

// Trait definition
trait Loggable
{
    private bool $debugMode = false;

    public function log(string $message): void
    {
        if ($this->debugMode) {
            echo "[LOG] " . date('Y-m-d H:i:s') . " - {$message}\n";
        }
    }

    public function enableDebug(): void
    {
        $this->debugMode = true;
    }
}

// Enum (PHP 8.1+)
enum HighlightScope: string
{
    case Keyword = 'keyword';
    case Function = 'function';
    case String = 'string';
    case Comment = 'comment';
    case Type = 'type';

    public function getColor(): string
    {
        return match ($this) {
            self::Keyword => '#ff79c6',
            self::Function => '#50fa7b',
            self::String => '#f1fa8c',
            self::Comment => '#6272a4',
            self::Type => '#8be9fd',
        };
    }
}

// Abstract class
abstract class BaseHighlighter implements LanguageParser
{
    use Loggable;

    protected array $captures = [];

    abstract protected function buildQuery(): string;

    public function parse(string $code): array
    {
        $this->log("Parsing code of length: " . strlen($code));

        $query = $this->buildQuery();
        // Simulated parsing
        return $this->processCaptures($code, $query);
    }

    private function processCaptures(string $code, string $query): array
    {
        $lines = explode("\n", $code);
        $result = [];

        foreach ($lines as $lineNum => $line) {
            $result[$lineNum] = [
                'content' => $line,
                'captures' => $this->analyzeLineCaptures($line),
            ];
        }

        return $result;
    }

    private function analyzeLineCaptures(string $line): array
    {
        $captures = [];

        // Simple keyword detection
        $keywords = ['function', 'class', 'interface', 'trait', 'enum', 'match'];
        foreach ($keywords as $keyword) {
            if (str_contains($line, $keyword)) {
                $captures[] = [
                    'scope' => HighlightScope::Keyword,
                    'text' => $keyword,
                ];
            }
        }

        return $captures;
    }
}

// Concrete class with attributes
#[Attribute]
class PHPHighlighter extends BaseHighlighter
{
    public function __construct(
        private readonly string $version = '8.2',
        private array $extensions = [],
    ) {
        $this->enableDebug();
    }

    public function getLanguageName(): string
    {
        return 'PHP ' . $this->version;
    }

    protected function buildQuery(): string
    {
        return <<<'QUERY'
        (function_definition
            name: (name) @function)
        (class_declaration
            name: (name) @type)
        QUERY;
    }

    public function addExtension(string $ext): self
    {
        $this->extensions[] = $ext;
        return $this;
    }
}

// Usage example
$highlighter = new PHPHighlighter(version: '8.2');
$highlighter->addExtension('tokenizer');

$code = <<<'PHP'
function greet(string $name): string {
    return "Hello, {$name}!";
}
PHP;

$result = $highlighter->parse($code);
print_r($result);
