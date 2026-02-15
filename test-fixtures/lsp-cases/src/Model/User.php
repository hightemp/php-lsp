<?php

declare(strict_types=1);

namespace App\Model;

// CASE: Base model for hover/definition/references and cross-file indexing.
class User
{
    // CASE: Property symbol for hover/document symbols/member references.
    private string $name;

    // CASE: Class constant reference target.
    public const TYPE = 'regular';

    // CASE: Static property reference target.
    public static int $created = 0;

    // CASE: Constructor with one required + one optional argument.
    public function __construct(string $name, ?int $age = null)
    {
        $this->name = $name;
        self::$created++;
    }

    // CASE: Method symbol + method call references.
    public function getName(): string
    {
        return $this->name;
    }

    // CASE: Static method for static completion and references.
    public static function fromName(string $name): self
    {
        return new self($name);
    }
}

