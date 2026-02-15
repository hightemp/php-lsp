<?php

declare(strict_types=1);

namespace App\Lsp;

// CASE: Global constant symbol.
const FIXTURE_VERSION = '1.0';

// CASE: Top-level function symbol.
function fixture_helper(string $value): string
{
    return strtoupper($value);
}

// CASE: Interface symbol.
interface Nameable
{
    public function name(): string;
}

// CASE: Trait symbol.
trait Timestamped
{
    public function now(): int
    {
        return time();
    }
}

// CASE: Enum + enum case symbols.
enum Mode: string
{
    case FAST = 'fast';
    case SAFE = 'safe';
}

// CASE: Class symbol with method/property/class-constant children for documentSymbol tree.
class SymbolContainer implements Nameable
{
    use Timestamped;

    public const KIND = 'container';
    private string $name = 'default';

    public function __construct(string $name = 'default')
    {
        $this->name = $name;
    }

    public function name(): string
    {
        return $this->name;
    }
}

