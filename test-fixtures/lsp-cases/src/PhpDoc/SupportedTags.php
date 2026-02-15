<?php

declare(strict_types=1);

namespace App\PhpDoc;

use App\Model\User;

/**
 * CASE: Class-level PHPDoc with summary and virtual members.
 * Multi-line summary should be merged into one sentence.
 *
 * @property string $label Human-readable label
 * @property-read int $version Read-only version marker
 * @property-write bool $dirty Write-only state marker
 * @method User findById(int $id)
 * @method static self make()
 */
class SupportedTags
{
    /**
     * CASE: Property @var with nullable and generic-like notation.
     * @var array<int, User>|null In-memory cache
     */
    private ?array $cache = null;

    /**
     * CASE: Method PHPDoc with typed and untyped params.
     * @param string $name Display name
     * @param ?int $age Optional age
     * @param $meta Untyped payload (allowed in parser)
     * @return User|null
     * @throws \InvalidArgumentException
     * @deprecated Use buildFromPayload() instead
     */
    public function build(string $name, ?int $age, $meta): ?User
    {
        if ($name === '') {
            throw new \InvalidArgumentException('name is required');
        }

        if ($age !== null && $age < 0) {
            return null;
        }

        $user = User::fromName($name);
        $this->cache = [1 => $user];
        return $user;
    }
}

/**
 * CASE: Function-level PHPDoc.
 * @param User $user
 * @return string
 * @throws \RuntimeException
 */
function renderUser(User $user): string
{
    $name = $user->getName();
    if ($name === '') {
        throw new \RuntimeException('empty name');
    }

    return $name;
}

