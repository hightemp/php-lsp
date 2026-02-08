<?php

declare(strict_types=1);

namespace App;

/**
 * A simple class for testing.
 */
class Foo
{
    private string $name;
    protected int $count = 0;

    public function __construct(string $name)
    {
        $this->name = $name;
    }

    /**
     * Get the name.
     *
     * @return string The name
     */
    public function getName(): string
    {
        return $this->name;
    }

    /**
     * Increment counter.
     *
     * @param int $amount Amount to add
     * @return void
     */
    public function increment(int $amount = 1): void
    {
        $this->count += $amount;
    }

    public static function create(string $name): self
    {
        return new self($name);
    }
}
